# Phase 11 · 无障碍（AccessKit）

> **P18 迁移**：demo 已并入 `..`，不再是独立 crate；本文中的
> `crates/ui_core_demo` 路径与命令是当时的历史记录，当前命令见 `design/phase-18-demo-example.md`。
>
> 上游：`custom-gui-engine-design.md` §7（全章）、§4.4（`hit_test`）、§8.4（焦点 / 滚动）、§9.1（诊断）、§1.3（公开面白名单）、
> §3.4（失效传播）；`design/phase-01-foundation.md` 附录 A.3 / §4 / §6.2；`design/phase-03-node-state.md`
> （`Node` / `A11Y` 脏位 / `ReadCtx`）；`design/phase-06-paint-runtime.md`（`RenderedFrame` / 帧门禁）；
> `design/phase-09-win32-demo.md`（`WndProc` / 泵 / `Dispatcher`）；`design/phase-10-interaction.md`（焦点 / 滚动 /
> `Action`）。
>
> **阶段号**：本文 = 实施序列的 **P11**（`design/phase-NN` 的文件序）。`custom-gui-engine-design.md` §7 与
> `phase-01-foundation.md` 附录 A.3 / 文件树里的「P14」是**旧路线图编号，指同一阶段**；`phase-03/05/06/10`
> 里出现的「P11」就是本文。DEVIATIONS 按 §十五（P10）之后追加为「十六、P11」。
>
> **本阶段只做设计 + API 核验 + 可编译 probe**：不写仓库代码、不改 `Cargo.toml` / `Cargo.lock`、不 commit。
> `accesskit` / `accesskit_windows` 已在 P01 的依赖冻结里落到 `..`
> （`[target.'cfg(windows)'.dependencies]`，`Cargo.toml:78-79`），**本阶段零依赖变更**。

---

## 0. 结论摘要（先给定死项）

| #  | 定死项                                                                                                                                                                                                                                        | 依据                           |
|----|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|--------------------------------|
| 1  | **`A11yTreeSnapshot` 进 `FrameRecord`（不是 `RenderedFrame`）**：`A11Y` 变化不产生 damage（§7.5 / P03 §9.3 表第 9 行），A11Y-only 变化会让 `App::frame` 返回 `FrameOutcome::Idle`，而 `RenderedFrame` 只存在于已提交帧（`window.rs:539-544`） | §9.1 / F-113                   |
| 2  | **a11y pass 在帧门禁之外**：每轮泵迭代调用一次（adapter 不活跃时 O(1)），不能挂在 `App::frame` 的 `needs_frame` / `commit_frame` 门禁里                                                                                                       | §8.2 / S-110                   |
| 3  | **`mark_layout` 同时置 `A11Y`**（bounds 变了 ⇒ a11y bounds 变了）；`A11Y` 是**累积位**，由 a11y pass 消费并 `clear_a11y`                                                                                                                      | §6 / S-108 / S-109             |
| 4  | **`bind_a11y` 在 `refresh_styles` 内求值**（同一 `ReadCtx`、同一依赖集）；不做独立 pass                                                                                                                                                       | §7.3 / S-107                   |
| 5  | **Windows 用 `Adapter` + 自有 `WndProc` 接 `WM_GETOBJECT`，不用 `SubclassingAdapter`**；`Adapter` 在 `NativeWindow::create` 里（`ShowWindow` **之前**）构造                                                                                   | §8.1 / probe ②                 |
| 6  | **`Node` 尺寸不变（1144 B）**：a11y 槽走与 `layout_cache` / `clip_keys` 同型的**侧表**，不塞进 `Node`                                                                                                                                         | §11.1 / F-114                  |
| 7  | **文本 → `Role::Label` + `set_value(text)`**（不是 `TextRun` / `StaticText`——后者不存在，前者被消费端当透明节点）                                                                                                                             | §4.1 / probe ① / F-106 / F-107 |
| 8  | **`Canvas` / `Geometry` → `Role::Image` + `label` = 替代文本；缺替代文本 ⇒ `set_hidden()` + 诊断**（`FontStackMiss` 式，不静默丢弃）                                                                                                          | §5.4 / F-109                   |
| 9  | **`TreeUpdate.focus` 每帧必带，且必须指向「本次已发出」的节点**（否则 `accesskit_consumer` `panic!`）；未发出（虚拟列表窗口外的行）⇒ 回退到最近的已发出祖先                                                                                   | §6.3 / F-110                   |
| 10 | **`ActionRequest` 只入队**（`ActionHandler` 可能在任意线程）→ `Dispatcher::push(UserTask)` → 主线程 drain → 标准失效闭环；陈旧 `NodeId` 忽略 + 诊断                                                                                           | §9 / P09 `dispatcher.rs:84-98` |

---

## 1. 范围、文件与非目标

### 1.1 本阶段交付（实现期落地的文件）

```text
crates/ui_core/src/
├── a11y.rs                       # 公共模块根：全部公开契约类型（A11yRole/A11yNode/A11yLabel/
│                                 #   A11yAction/A11yOverride/A11yPatch/A11yNodeId/A11yTreeSnapshot/
│                                 #   A11yDiagnostic/A11yActionCtx/A11yActionData）
├── a11y/
│   ├── tree.rs                   # A11yTree（每窗口状态机）：变更集合、快照构建、A11Y/ORDER 消费
│   ├── mapping.rs                # A11yRole ↔ accesskit::Role、A11yAction ↔ accesskit::Action、
│   │                             #   bounds 换算、hidden/disabled/actions 判据（纯函数 + 表）
│   ├── adapter.rs                # A11yBridge：accesskit::Node 构造、TreeUpdate 组装、焦点规则、懒激活
│   └── action.rs                 # ActionRequest → 引擎动作（焦点 / 滚动 / 声明动作 / 应用 handler）
└── platform/windows/a11y.rs       # Windows 直连：Adapter 构造 / WM_GETOBJECT / 窗口焦点 / raise()
```

改动（**不改签名语义，只加继电器**）：`lib.rs`（+`pub mod a11y;`）、`app.rs`（`format_a11y` 入口 + `a11y_diagnostics`

+ `on_a11y_action`）、`window.rs`（`a11y: A11yBridge` 字段 + `set_scale` 换代）、`node/builder.rs`
  （`a11y` / `bind_a11y`）、`node/tree.rs`（`a11y_slots` 侧表 + `clear_a11y` + `a11y_dirty()` 计数）、
  `node/dirty.rs`（`mark_layout` 集合加 `A11Y`）、`node/tree.rs`/`node/dependency.rs`（无新类型）、
  `paint/pass.rs`（无改动）、`platform/windows/{event_loop,window}.rs`（`WM_GETOBJECT` /
  `WM_SETFOCUS` / `WM_KILLFOCUS` 分支 + 每轮 a11y 调用）、`error.rs`（`A11yInit { detail }`，phase-01 §6.2.2 变体 #17 的落地）、
  `test_support/{record,app}.rs`（`FrameRecord.a11y` + 捕获开关）。

**不建**：`a11y/test_support.rs`（单测内联在被测文件末尾，M-04）；`a11y/tests.rs`（AGENTS §5.1）。

### 1.2 明确的非目标

| 不做                                                       | 归属                                                                                                                                     |
|------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------|
| macOS `NSAccessibility` / Linux AT-SPI adapter             | P12（本文 §12 只留接口与文件位置）                                                                                                       |
| 真实辅助技术联动验收（NVDA / Narrator / VoiceOver / Orca） | L5 实机矩阵（`phase-10` 的 V 系列同口径）                                                                                                |
| `Role` 全集映射（accesskit 0.25.2 有约 200 个变体）        | 永久：只映射引擎真实会产生的角色（§4.1 的 27 个）                                                                                        |
| 文本编辑模型（`SetValue` 的落地写哪个信号）                | 应用层：引擎只提供 `on_a11y_action` + `ActionCtx` 通道                                                                                   |
| `SetTextSelection` 的字符索引 → 字节 → 字形映射            | 应用层（P05 的 `text_selection_rects` 已给几何，见 `tree.rs:1167`）                                                                      |
| 子树 a11y（accesskit 的 `tree_id` / graft）                | 不做：单窗口单根树（`TreeId::ROOT`），与 §8.4「v1 单窗口」一致                                                                           |
| UIA `ITextProvider` / `IValueProvider` 的自定义实现        | 全部交给 `accesskit_windows`（它按角色/属性自动挂 pattern）                                                                              |
| a11y hit-test 的自定义路径                                 | Windows 走 UIA fragment（`handle_wm_getobject` 返回的 `IRawElementProviderSimple`），引擎侧只保证 `layout::hit_test` 与 a11y bounds 同源 |

---

## 2. API 核验（`accesskit 0.25.2` / `accesskit_windows 0.35.2` / `accesskit_consumer 0.39.2`）

> 版本取自仓库 `Cargo.lock`：`accesskit 0.25.2`、`accesskit_windows 0.35.2`、`accesskit_consumer 0.39.2`
> （后者是 `accesskit_windows` 的直接依赖，**它才是真正校验 `TreeUpdate` 的一方**，必须一起核）。
> 源码路径：`E:\.cargo\registry\src\rsproxy.cn-e3de039b2554c837\<crate>-<ver>\src\`。行号为该版本实测。

### 2.1 `accesskit 0.25.2`

```rust
// lib.rs:11   — 无 pyo3/schemars 时 crate 是 #![no_std]（引擎用默认 features，无 serde）
// lib.rs:32/35 — pub use uuid::Uuid; pub use geometry::{Affine, Point, Rect, Size, Vec2};

pub enum Role { … }                                    // :62，约 200 个变体
pub enum Action {                                      // :289
    Click,
    Focus,
    Blur,
    Collapse,
    Expand,
    CustomAction,
    Decrement,
    Increment,
    HideTooltip,
    ShowTooltip,
    ReplaceSelectedText,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
    ScrollUp,
    ScrollIntoView,
    ScrollToPoint,
    SetScrollOffset,
    SetTextSelection,
    SetSequentialFocusNavigationStartingPoint,
    SetValue,
    ShowContextMenu,
}                                                      // :291-349，共 22 个（不是 26）
pub type NodeIdContent = u64;                          // :703
pub struct NodeId(pub NodeIdContent);                  // :715 —— 字段 pub，可直接打包引擎代际
pub struct TreeId(pub Uuid);                           // :745
impl TreeId { pub const ROOT: Self = Self(Uuid::nil()); } // :749
pub struct CustomAction {
    pub id: i32,
    pub description: String
}  // :761
pub struct TextPosition {
    pub node: NodeId,
    pub character_index: usize
}  // :771
pub struct TextSelection {
    pub anchor: TextPosition,
    pub focus: TextPosition
} // :784
pub struct Node {
    role,
    actions,
    child_actions,
    flags,
    properties
}  // :1107（字段全私有）
pub struct TreeInfo {
    pub root: NodeId,
    toolkit_name: Option<String>,
    toolkit_version: Option<String>
}             // :3173；new(root) :3187
pub type Tree = TreeInfo;                              // :3182-3183（deprecated，别用）
pub struct TreeUpdate {
    pub nodes: Vec<(NodeId, Node)>,
    pub tree: Option<TreeInfo>,    // :3239
    pub tree_id: TreeId,           // :3250
    pub focus: NodeId
}            // :3261（"必须每次携带"）
pub enum ScrollUnit { Item, Page }                     // :3275
pub enum ScrollHint { TopLeft, BottomRight, TopEdge, BottomEdge, LeftEdge, RightEdge } // :3295
pub enum ActionData {
    CustomAction(i32),
    Value(Box<str>),
    NumericValue(f64),
    ScrollUnit(..),
    ScrollHint(..),
    ScrollToPoint(Point),
    SetScrollOffset(Point),
    SetTextSelection(TextSelection)
}            // :3309
pub struct ActionRequest {
    pub action: Action,
    pub target_tree: TreeId,
    pub target_node: NodeId,
    pub data: Option<ActionData>
} // :3332
pub trait ActivationHandler { fn request_initial_tree(&mut self) -> Option<TreeUpdate>; } // :3340/:3362
pub trait ActionHandler { fn do_action(&mut self, request: ActionRequest); }          // :3366/:3376
pub trait DeactivationHandler { fn deactivate_accessibility(&mut self); }                // :3380/:3388
```

`Node` 的**真实**写面（宏生成，行号是宏调用点；每个属性都有 getter / setter / clearer 三件套）：

| 类别                         | 方法（本阶段用到的）                                                                                         | 行                                      |
|------------------------------|--------------------------------------------------------------------------------------------------------------|-----------------------------------------|
| 构造 / 角色 / 动作           | `Node::new(role)`、`set_role(Role)`、`add_action(Action)`、`remove_action`、`clear_actions`                  | 1828 / 1862 / 1871 / 1875 / 1879        |
| bool 旗标（`flag_methods!`） | `set_hidden`、`set_disabled`、`set_read_only`、`set_clips_children`、`set_modal`、`set_selected`             | 1919 / 1958 / 1953 / 1962 / 1945 / 2453 |
| 子节点                       | `set_children(impl Into<Vec<NodeId>>)`、`push_child(NodeId)`、`clear_children`                               | 2028（`vec_property_methods!` :1319）   |
| 字符串                       | `set_label(impl Into<String>)`、`set_value(impl Into<String>)`、`set_description(..)`、`set_placeholder(..)` | 2104 / 2122 / 2109 / 2165               |
| 数值                         | `set_numeric_value(f64)`                                                                                     | 2228                                    |
| 几何                         | `set_bounds(Rect)`（`Rect { x0, y0, x1, y1 }: f64`，`geometry.rs:337`）                                      | 2537                                    |
| 文本选区                     | `set_text_selection(TextSelection)`                                                                          | 2540                                    |

**`Role` 子集（本阶段只用这 27 个，逐条已编译验证）**：
`Window`(207)、`Application`(120)、`GenericContainer`(85)、`Group`(152)、`Label`(67)、`Paragraph`(80)、
`Heading`(154)、`Button`(90)、`TextInput`(89)、`MultilineTextInput`(104)、`CheckBox`(87)、`Switch`(101)、
`RadioButton`(88)、`Slider`(187)、`ProgressIndicator`(175)、`List`(96)、`ListBoxOption`(77)、`Table`(97)、
`Grid`(150)、`ScrollView`(182)、`ScrollBar`(181)、`Image`(68)、`Menu`(102)、`MenuBar`(167)、`MenuItem`(78)、
`Dialog`(140)、`Tooltip`(202)。

**不存在**（主设计 §7.4 表写错的两条，见 F-106）：

- `Role::StaticText` —— **没有**（`Role` 里与文本相关的只有 `TextRun`(65)、`Label`(67)、`Paragraph`(80)、`LineBreak`(160)、
  `Code`(128) 等）。
- `Role::ScrollArea` —— **没有**，对应的是 `ScrollView`(182)。

### 2.2 `accesskit_windows 0.35.2`（Windows adapter）

```rust
// lib.rs:14  pub use adapter::{Adapter, QueuedEvents};
// lib.rs:17  pub use subclass::SubclassingAdapter;
// lib.rs:19  pub use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
//            ⇒ 与引擎的 windows 0.62.2 **同一套 HWND/LPARAM/WPARAM 类型**（probe ② 实测可互传）

impl Adapter {
    /// 文档（adapter.rs:411-422）：action handler 可能不在窗口线程；**不得在处理 WM_GETOBJECT 时构造**
    /// （必须先 init UIA，否则嵌套 WM_GETOBJECT 会让 AT 认为窗口不原生支持 UIA）。
    pub fn new(hwnd: HWND, is_window_focused: bool,
               action_handler: impl 'static + ActionHandler + Send) -> Self;     // :423
    pub fn update_if_active(&mut self,
                            update_factory: impl FnOnce() -> TreeUpdate) -> Option<QueuedEvents>; // :463
    pub fn update_window_focus_state(&mut self, is_focused: bool) -> Option<QueuedEvents>;       // :500
    pub fn handle_wm_getobject<H: ActivationHandler + ?Sized>(
        &mut self, wparam: WPARAM, lparam: LPARAM, activation_handler: &mut H,
    ) -> Option<impl Into<LRESULT> + use < H >>;                                 // :534（**三参数**）
}
const PLACEHOLDER_ROOT_ID: NodeId = NodeId(0);                                // :374
fn init_uia() { … UiaLookupId(..) }                                          // :596
#[must_use]
pub struct QueuedEvents(Vec<QueuedEvent>);                       // :630
impl QueuedEvents { pub fn raise(self) }                                     // :643
unsafe impl Send for QueuedEvents {}                                         // :673
```

`State` 三态与返回值语义（`adapter.rs:467-489`）：

| 状态                                                | `update_if_active`   | 闭包是否执行                                   |
|-----------------------------------------------------|----------------------|------------------------------------------------|
| `Inactive`（从未有 `WM_GETOBJECT`）                 | `None`               | **不执行** ⇒ 懒激活零开销                      |
| `Placeholder`（`request_initial_tree` 返回 `None`） | `Some(QueuedEvents)` | 执行，且**必须是全树**（`adapter.rs:451-456`） |
| `Active`                                            | `Some(QueuedEvents)` | 执行，可增量                                   |

`SubclassingAdapter`（`subclass.rs`）—— **本阶段不用**（§8.1 给理由），但它的实现是接线纪律的参考：

- `new(hwnd, activation_handler, action_handler)`（:162）；窗口已可见 ⇒ `panic!("…must be created before the
  window is shown…")`（:167-171）。
- 它自己装 `wnd_proc`：`WM_GETOBJECT`(50)、`WM_SETFOCUS`/`WM_EXITMENULOOP`/`WM_EXITSIZEMOVE` → focused=true (62-64)、
  `WM_KILLFOCUS`/`WM_ENTERMENULOOP`/`WM_ENTERSIZEMOVE` → false (65-67)、`WM_NCDESTROY` 记账 (68-70)。
- **关键纪律**：`handle_wm_getobject` 的返回值在 `state.borrow_mut()` **释放之后**才 `.into()`（:50-60）——
  因为 `UiaReturnRawElementProvider` 会**同步派发嵌套 `WM_GETOBJECT`**（`adapter.rs:530-533` 的注释）。
  引擎的 `WndProc` 必须照做（§8.2）。
- `Drop` 里 `uninstall()`（:200-204）。

**没有 deactivation**：`accesskit_windows 0.35.2` 全仓无 `deactivate` 字样，`Adapter` 无 `Drop` 实现
（只有 `SubclassingAdapter` 的 `Drop` 用来卸子类化）。⇒ Windows 侧「关闭时 deactivate」= **销毁窗口后 drop `Adapter`**；
`DeactivationHandler` 只属于 `accesskit_unix`（F-111）。

### 2.3 `accesskit_consumer 0.39.2`（`TreeUpdate` 的真实校验规则 —— 增量算法的**唯一**依据）

| 规则                                                                                                                                                                                                  | 源码位置                                                            | 对引擎的强制含义                                                                                                                                                     |
|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|---------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **根与焦点必须在节点表里**：`validate_global` 对 `self.root` / `self.focus` 查 `nodes`，缺一即 `panic!`                                                                                               | `tree.rs:86-96`（在 `update` 末尾 :485 调用）                       | 首次全树必须含 root + focus；之后每次 `focus` 必须在树内 ⇒ §6.3 的回退规则                                                                                           |
| 每个 `nodes` 条目必须**可达**：是 root，或本次 `nodes` 里某节点的 child，或**已在树里**的某节点的 child；否则 `panic!("…neither in the current tree nor a child of another node…")`                   | `tree.rs:317-323`（`pending_nodes`）                                | **新增子节点必须同批发父节点**（父的 `children` 里带上它）                                                                                                           |
| `children` 里出现的 id 必须可达，否则 `panic`（`pending_children`）                                                                                                                                   | `tree.rs:324-330`                                                   | 父的 `children` 不能引用未发出的**新**子节点                                                                                                                         |
| **重复 child** ⇒ `panic!("TreeUpdate includes duplicate child …")`                                                                                                                                    | `tree.rs:233-237`                                                   | 一个 id 在一次 update 里只能出现在**一个**父的 `children` 中（树天然满足；重发多个父时不重复）                                                                       |
| **删除**：把父重新发出、`children` 里去掉该子，**且不要把被删节点放进 `nodes`** ⇒ 整棵子树被回收                                                                                                      | `tree.rs:265-270`（`unreachable`）→ `traverse_unreachable` :353-393 | 「卸载 = 重发父的 children」，被删节点**不出现**（§7.5 原意，已实测）                                                                                                |
| 已存在节点的更新**不需要**父节点                                                                                                                                                                      | `tree.rs:261-293`                                                   | 单改 label 的增量可以只含 1 个节点（probe ② 实测 `updated == [button]`）                                                                                             |
| 更新的节点若是新 id 且无父 ⇒ 进 `pending_nodes` ⇒ panic                                                                                                                                               | 同上                                                                | 新节点必须带父                                                                                                                                                       |
| `Tree::new` 要求 `tree: Some(..)` 且 `tree_id == TreeId::ROOT`                                                                                                                                        | `tree.rs:592-600`                                                   | 首次必须给 `TreeInfo`；不做子树                                                                                                                                      |
| `focus` 只在 `host_focused` 时才参与 `focus_id()`；焦点移动触发 `focus_moved` 回调                                                                                                                    | `tree.rs:534-541` / :650-672                                        | 焦点用 `update_window_focus_state(true)` 打开                                                                                                                        |
| **隐藏节点的过滤**：`common_filter_base` **先判 `is_focused()`**（focused ⇒ `Include`），再判 `is_hidden()`（⇒ `ExcludeSubtree`）；`GenericContainer` / `TextRun` ⇒ `ExcludeNode`（透明，子节点上提） | `filters.rs:17-37`                                                  | ①`set_hidden(true)` = 从 AT 树里排除（除非它恰好有焦点）；②`Box → GenericContainer` 的容器**不出现**在 UIA 树里，其子节点上提到祖父；③`TextRun` 不能当普通文本叶子用 |
| `Label` 角色的可访问名来自 **`value`**：`label_comes_from_value()` ⇔ `role() == Label`；`write_label` 对 `Label` 走 `write_value`                                                                     | `node.rs:756-763` / :787-804                                        | 文本节点必须 `Label` + `set_value`，**不能**用 `label`                                                                                                               |
| 适配器侧印证：`name()` 按 `label_comes_from_value()` 分派 `write_value`，`Role::Label ⇒ UIA_TextControlTypeId`                                                                                        | `accesskit_windows/node.rs:406-409` / :65                           | 同上（UIA Name = 文本内容）                                                                                                                                          |

### 2.4 与主设计 §7.6 / 附录 A.3 的差异汇总

| 设计原文                                               | 实测                                                                | 处置                                      |
|--------------------------------------------------------|---------------------------------------------------------------------|-------------------------------------------|
| §7.6 `adapter.handle_wm_getobject(wparam)`             | **三参数** `(wparam, lparam, &mut activation_handler)`              | 附录 A.3 已记；本文 §8.2 按三参数接线     |
| §7.6 `adapter.update_if_active(                        |                                                                     | update) → QueuedEvents::raise()`          | Windows：`Option<QueuedEvents>`；unix：无返回值 | 本文只做 Windows；unix 归 P12 |
| §7.4 文本 → `TextRun` / `StaticText`，`label` = 文本   | `StaticText` 不存在；`TextRun` 是透明节点且要求 `character_lengths` | **改 `Label` + `value`**（F-106 / F-107） |
| §7.4 滚动容器 → `ScrollArea`                           | 不存在，是 `ScrollView`                                             | 改 `ScrollView`（F-106）                  |
| §7.4 `Canvas`/`Geometry` → `Image` / `GraphicsObject`  | `GraphicsObject` 需要 `GraphicsDocument` 祖先（ARIA graphics 模块） | **只 `Image`**（F-109）                   |
| §7.4「`hidden` 同时从树与命中测试中排除」              | 消费端对**被聚焦**的 hidden 节点反而 `Include`（焦点可达保证）      | 记录（F-110），语义按实测                 |
| §7.5「卸载节点：发送父节点 children 更新」             | 与消费端完全一致（probe ② 复现 panic 于「只发子不发父」）           | 保留                                      |
| §8.1 shutdown「a11y deactivate」                       | Windows 无 deactivation API                                         | 改为 drop `Adapter`（F-111）              |
| §7.9「`A11yTreeSnapshot` 是 `RenderedFrame` 的一部分」 | Idle 帧没有 `RenderedFrame`                                         | 改 `FrameRecord`（F-113）                 |

### 2.5 probe 记录（`%TEMP%\kg-p11-a11y`，仓库外，2026-11-13）

> 环境：Windows，非交互式 agent 会话；`cargo run --offline`（crate 源在本地 registry 缓存）；
> `CARGO_TARGET_DIR=%TEMP%\kg-p11-a11y-target`。两个 bin 全部通过。

**probe ① `cpu_tree`（无 HWND，纯 CPU 树构建 → 交给真正的消费端）**

用的是**真实的 `accesskit 0.25.2` 数据类型 + 真实的 `accesskit_consumer 0.39.2`**（`accesskit_windows` 内部就是它）。
11 个节点的树（Window 根 / GenericContainer / Label 文本 / Button / TextInput / ScrollView + 2 行 / Image 自绘 / hidden
按钮）：

| #  | 断言                                                                                                                  | 结果                                                                                      |
|----|-----------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------|
| 1  | `Tree::new` 接受全树，`toolkit_name=="Kytrix"`、`toolkit_version==Some("0.0.3")`、`root`/`focus` 就位                 | ✅                                                                                        |
| 2  | `root.children() == [container, scroll, hidden]`（**未过滤**：全树所有 mounted 节点都在）                             | ✅                                                                                        |
| 3  | `root.filtered_children(common_filter) == [text, button, input, canvas, scroll]`：容器被上提、hidden 子树消失         | ✅（发现 F-108）                                                                          |
| 4  | `common_filter(container) == ExcludeNode`（`GenericContainer` 透明）                                                  | ✅                                                                                        |
| 5  | `Label` 节点：`label() == None`、`label_comes_from_value() == true`、`value() == Some("价格")`                        | ✅（发现 F-107）                                                                          |
| 6  | `Button`：`role/label()/supports_action(Click                                                                         | Focus)` 正确；`raw_bounds == Rect::new(10,40,85,70)`（逻辑 8/32/60/24 × 1.25 逐边 round） | ✅ |
| 7  | `TextInput`：`value()=="1"`、`supports_action(SetValue                                                                | SetTextSelection)`                                                                        | ✅ |
| 8  | **只改 label 的增量只含 1 个节点**，`updated == [button]`、`added == []`                                              | ✅                                                                                        |
| 9  | **新增子节点**必须同批发父（`added` 含子、父 `children` 变 3）；**只发子不发父 ⇒ 消费端 panic**                       | ✅（捕获 panic）                                                                          |
| 10 | **删除**：只发父（children 缩短）、被删节点不出现 ⇒ `removed` 含子、父 `children` 回到 2                              | ✅                                                                                        |
| 11 | **只改焦点**：`nodes: []` 合法，`focus_moved == [(root, button)]`                                                     | ✅                                                                                        |
| 12 | 焦点指向不存在的 id ⇒ `panic!("Focused ID … is not in the node list")`                                                | ✅（捕获 panic）                                                                          |
| 13 | hidden 节点**未被聚焦时**不在 filtered children；**被聚焦后**反而 `Include`                                           | ✅（发现 F-110）                                                                          |
| 14 | **陈旧 NodeId**（同槽位、generation+1）⇒ `node_by_tree_local_id(..).is_none()`                                        | ✅                                                                                        |
| 15 | `ActionRequest{SetValue, Value("42")}` 经 `impl ActionHandler` 收到；`TextSelection` / `ActivationHandler` 形状可构造 | ✅                                                                                        |
| 16 | 27 个 `Role` 变体逐条 `Node::new(role).role() == role`                                                                | ✅                                                                                        |

**probe ② `win_adapter`（Windows adapter 的真实构造与惰性）**

隐藏窗口（`IsWindowVisible == false`）+ 自有 `WndProc`（`DefWindowProcW` 兜底）：

| # | 事实                                                                                                 | 结果 |
|---|------------------------------------------------------------------------------------------------------|------|
| 1 | `Adapter::new(hwnd, false, handler)` 在 **首显之前**构造成功（`SubclassingAdapter` 在此处会 panic）  | ✅   |
| 2 | Inactive（显示前）`update_if_active(..)` 返回 `None`，**闭包执行 0 次**                              | ✅   |
| 3 | `update_window_focus_state(true)` 在 Inactive 下返回 `None`                                          | ✅   |
| 4 | `ShowWindow(SW_SHOWNORMAL)` 之后、仍无 AT ⇒ `update_if_active` 返回 `None`，闭包仍 **0 次**          | ✅   |
| 5 | `handle_wm_getobject(WPARAM(0), LPARAM(OBJID_CLIENT=-25), &mut activation)` → `Some(..)`（占位激活） | ✅   |
| 6 | 占位激活后 `update_if_active` → `Some(QueuedEvents)`，闭包执行 **1 次**（首棵全树）                  | ✅   |
| 7 | 非 `OBJID_CLIENT`/`UiaRootObjectId` 的 objid ⇒ `None`（交 `DefWindowProcW`）                         | ✅   |
| 8 | `QueuedEvents::raise` 可作为 `fn(QueuedEvents)` 取到（本 probe 刻意不调用：无 AT 时不向 UIA 广播）   | ✅   |

**probe ③ `wm_getobject_pattern`（编译期证据：借用作用域）**

把 §8.2 的分发函数逐字写成独立 bin，证明「`&mut` 借用作用域只覆盖 `handle_wm_getobject`、
`Option<impl Into<LRESULT>> → LRESULT` 的转换在作用域**之外**」能编译（`use<H>` 不把
activation handler 的生命周期绑到返回值上）。这与 `SubclassingAdapter` 的既有写法同构
（`subclass.rs:50-60` 先 `drop(state)` 再 `result.into()`）——是 `WndProc` 接线不产生悬垂/重入借用的依据。
结果：✅（`cargo run --offline --bin wm_getobject_pattern` 输出通过）。

**probe 的边界（不声称验证）**：UIA 侧的角色/名称/动作映射、`raise()` 在真实 AT 下的效果、`WM_GETOBJECT`
的真实来源（无辅助技术连接）。这些属 L5（V-75 / V-77）。

---

## 3. 模块结构与公开面

### 3.1 文件职责与非 test 消费者（dead_code 审计表）

| 文件                       | 内容                                                                                                     | 非 test 消费者                                                                         |
|----------------------------|----------------------------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------|
| `a11y.rs`                  | 全部公开契约类型（§4）+ `pub(crate) mod action/adapter/mapping/tree`                                     | 应用层（`NodeBuilder::a11y` 的参数类型）、`test_support`                               |
| `a11y/tree.rs`             | `A11yTree`（每窗口 a11y 状态机）：`collect` / `clear` / `NodeA11y` 侧表读写                              | `a11y/adapter.rs`、`App::frame`（捕获路径）                                            |
| `a11y/mapping.rs`          | `role_of` / `actions_of` / `bounds_of` / `hidden_of` / `disabled_of` / 诊断                              | `a11y/tree.rs`                                                                         |
| `a11y/adapter.rs`          | `A11yBridge`：`build_update`（`TreeUpdate`）、`take_snapshot`、`full_pending`、`capture`                 | `platform/windows/window.rs`（泵）、`App::frame`（捕获）、`a11y/action.rs`（代际校验） |
| `a11y/action.rs`           | `dispatch(app, window, request)`：`ActionRequest` → 引擎动作                                             | `platform/dispatcher.rs` 的 `UserTask`（主线程 drain）                                 |
| `platform/windows/a11y.rs` | `WindowsA11y`（`Adapter` + `WindowsActionHandler`）：`new` / `wm_getobject` / `focus_changed` / `update` | `platform/windows/{window,event_loop}.rs`                                              |

### 3.2 公开面（路径按模块根/叶子文件规则）

| 类型                                                                                        | 公开路径                          | 定义处                   | 白名单依据                                                                                |
|---------------------------------------------------------------------------------------------|-----------------------------------|--------------------------|-------------------------------------------------------------------------------------------|
| `A11yRole` `A11yNode` `A11yLabel` `A11yLabelSource` `A11yAction` `A11yOverride` `A11yPatch` | `ui_core::a11y::*`                | `src/a11y.rs`            | §1.3 白名单（phase-01 §4.5 已冻结这 6 个，本文加 `A11yLabelSource`）                      |
| `A11yNodeId`                                                                                | `ui_core::a11y::A11yNodeId`       | `src/a11y.rs`            | 同上（`A11yPatch` 的字段类型，必须可命名）                                                |
| `A11yTreeSnapshot`                                                                          | `ui_core::a11y::A11yTreeSnapshot` | `src/a11y.rs`            | **扩展** S-16 的传递闭包：`FrameRecord.a11y` 的字段类型必须 `pub`（`private_interfaces`） |
| `A11yActionCtx` `A11yActionData` `A11yDiagnostic`                                           | `ui_core::a11y::*`                | `src/a11y.rs`            | `App::on_a11y_action` / `App::a11y_diagnostics` 的签名组成                                |
| `A11yBridge` `A11yTree` `NodeA11y`                                                          | **不公开**（`pub(crate)`）        | `a11y/{adapter,tree}.rs` | 内部状态                                                                                  |

`A11yBridge` / `A11yTree` 是 `pub(crate)`；`NodeA11y`（侧表载荷）是 `pub(crate)`。**不新增 `pub use`**（M-02/M-07：
只允许 `pub(crate) use`）。**不新增 trait / 枚举分派**（I1 / I14）：`a11y` 的分平台部分只有
`platform/{windows,macos,linux}/a11y.rs` 三个**同名具体类型**，由 `platform/mod.rs` 的 `use … as native` 选择
（M-06 的既有形态）。

### 3.3 与 `Selector::Role` 的关系（F-63 的收口）

`test_support::probe::Selector` **不加** `Role(A11yRole)` 变体（V-1505 源头：`DEVIATIONS` F-63 说「延后 P14」）。
理由：角色是 a11y pass 的**产物**，不在 `Node` 上；给 `Probe` 加 `Role` 会让它必须在内部跑一遍 a11y 映射
（`Probe` 只有 `&NodeTree`，没有 `Window`/`A11yBridge`）。查询走 `A11yTreeSnapshot`：

```rust
let node = probe.by_selector(Selector::Id("submit")).unwrap();
assert_eq!(snap.node_for(node).unwrap().role(), A11yRole::Button);
```

---

## 4. 数据模型

### 4.1 `A11yRole` 与 `accesskit::Role` 的映射

引擎自己的枚举（**不 re-export `accesskit::Role`**：那会把 130+ 个变体与 accesskit 的类型身份塞进应用层，
且 `Role` 的变体在 0.25→0.26 已经改过名）。27 个变体，一对一：

| `A11yRole`                            | `accesskit::Role`        | 备注                                      |
|---------------------------------------|--------------------------|-------------------------------------------|
| `Window`                              | `Window`                 | 窗口根（`TreeInfo.root`）                 |
| `Application`                         | `Application`            | 应用显式声明（少见）                      |
| `GenericContainer`                    | `GenericContainer`       | 纯布局容器；**消费端视为透明**（F-108）   |
| `Group`                               | `Group`                  | 语义分组（容器要"出现"在 AT 树里时用）    |
| `Label`                               | `Label`                  | 文本叶子；可访问名走 `value`（F-107）     |
| `Paragraph` / `Heading`               | `Paragraph` / `Heading`  | 文本容器语义                              |
| `Button`                              | `Button`                 |                                           |
| `TextInput` / `MultilineTextInput`    | 同名                     |                                           |
| `CheckBox` / `Switch` / `RadioButton` | 同名                     |                                           |
| `Slider` / `ProgressIndicator`        | 同名                     |                                           |
| `List` / `ListBoxOption`              | `List` / `ListBoxOption` | 虚拟列表：容器 `List`、行 `ListBoxOption` |
| `Table` / `Grid`                      | 同名                     |                                           |
| `ScrollView` / `ScrollBar`            | 同名                     | **不是 `ScrollArea`**（F-106）            |
| `Image`                               | `Image`                  | `Canvas` / `Geometry` 的唯一落点（F-109） |
| `Menu` / `MenuBar` / `MenuItem`       | 同名                     | 浮层菜单                                  |
| `Dialog` / `Tooltip`                  | 同名                     | 浮层的另两种形态                          |

映射函数在 `a11y/mapping.rs`（`const fn role_to_accesskit(A11yRole) -> Role`），**穷举 `match`，无 `_` 分支**
（加变体时编译失败，不会静默变成 `Role::Unknown`）。

### 4.2 `A11yNode` / `A11yLabel` / `A11yAction`

```rust
/// 快照里的一个 a11y 节点（值语义；`A11yTreeSnapshot` 的载荷）。
#[derive(Clone, Debug, PartialEq)]
pub struct A11yNode {
    pub id: A11yNodeId,
    /// 反向定位到引擎节点（测试/诊断的唯一桥梁；`A11yNodeId` 本身也能解出来，见 §4.4）。
    pub engine: crate::node::id::NodeId,
    pub role: A11yRole,
    /// 可访问名。`None` = 无名（诊断会记 `MissingLabel` 若它是交互节点）。
    pub label: Option<A11yLabel>,
    /// 值（输入框内容 / `Role::Label` 的文本 / 数值控件的文本形态）。
    pub value: Option<SmolStr>,
    /// 补充说明（`aria-description` 语义）。
    pub description: Option<SmolStr>,
    /// **物理像素**、窗口相对（= `LayoutBox.bounds` 屏幕化后 × `scale`，逐边 round）。
    pub bounds: crate::math::PhysicalRect,
    pub actions: Vec<A11yAction>,
    pub hidden: bool,
    pub disabled: bool,
    /// 勾选/开关状态（`set_toggled`）。`RadioButton` / `CheckBox` / `Switch` 的选中态走这里。
    pub checked: Option<bool>,
    pub selected: Option<bool>,
    pub expanded: Option<bool>,
    /// 引擎侧的**直接**子节点（未过滤；hidden 子树故在列）。
    pub children: Vec<A11yNodeId>,
    pub parent: Option<A11yNodeId>,
}

impl A11yNode {
    /// 可访问名的文本（`None` = 无名）。
    pub fn label_text(&self) -> Option<&str>;
}

/// 可访问名 + **来源**：来源决定 accesskit 侧写 `set_label` 还是 `set_value`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct A11yLabel {
    text: SmolStr,
    source: A11yLabelSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum A11yLabelSource {
    /// 应用显式给出（`A11yOverride::label` / `A11yPatch::label`）⇒ `set_label`。
    Override,
    /// 来自 `NodeKind::Text` 的文本 ⇒ `set_value`（`Role::Label` 的可访问名走 value，F-107）。
    TextContent,
}
```

为什么 `A11yLabel` 不是 `Option<SmolStr>`：①来源决定 accesskit 侧的写入通道（`label` vs `value`），这是 **行为差异**不是实现细节；②
`§7.4` 要求 `Canvas` 缺替代文本时「不静默丢弃」——诊断要能说清「替代文本是谁给的」；
③测试要能断言「这个 Button 的名字来自 override，而不是从子文本推断」（引擎**不做**名字推断，见 §5.2）。

```rust
/// 引擎对外暴露（并接受）的 a11y 动作子集。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum A11yAction {
    Click,
    Focus,
    SetValue,
    SetTextSelection,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
    ScrollIntoView,
    SetScrollOffset,
    Expand,
    Collapse,
    ShowContextMenu,
    /// `accesskit::CustomAction` 的 id（描述由应用在注册时给）。
    Custom(i32),
}
```

`A11yAction` 是 `Copy + Eq + Hash`（handler 表的键，与 P10 的 `Action` 同纪律：**payload 不进键**）。
未列入的 `accesskit::Action`（`Blur`、`Decrement`、`Increment`、`HideTooltip`、`ShowTooltip`、
`ReplaceSelectedText`、`ScrollToPoint`、`SetSequentialFocusNavigationStartingPoint`）在 `action.rs` 里 **显式忽略 + 诊断**（
`UnhandledAction`），不做静默 no-op。

### 4.3 `A11yOverride`（静态）与 `A11yPatch`（动态）

```rust
/// 挂载时一次写入的静态 a11y 声明（`NodeBuilder::a11y`）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct A11yOverride {
    /// 覆盖由节点种类推导出的角色（§5.2）——唯一的"强制角色"入口。
    pub role: Option<A11yRole>,
    /// 替代文本 / 可访问名（`Canvas` / `Geometry` **必须**给，见 §5.4）。
    pub label: Option<SmolStr>,
    pub value: Option<SmolStr>,
    pub description: Option<SmolStr>,
    /// 显式动作表；`None`/空 ⇒ 按角色默认（§5.3）。
    pub actions: Option<Vec<A11yAction>>,
    /// 应用强制隐藏（例如"不可见但保留布局"的节点）。
    pub hidden: bool,
    /// 应用强制禁用（等价于 `StylePatch.interactive = Some(false)` 的 a11y 侧；§7.4）。
    pub disabled: bool,
}

/// `bind_a11y` 每帧求值的动态补丁（值语义；`None` = 本帧不覆盖）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct A11yPatch {
    pub role: Option<A11yRole>,
    pub label: Option<SmolStr>,
    pub value: Option<SmolStr>,
    pub description: Option<SmolStr>,
    pub disabled: Option<bool>,
    pub hidden: Option<bool>,
    /// 勾选/开关状态（`Toggled`）。与 `selected` **不是**可互换的通道：UIA 的 RadioButton
    /// 只按 `toggled()` 暴露 `SelectionItemPattern`（P15 F-151）。
    pub checked: Option<bool>,
    pub selected: Option<bool>,
    pub expanded: Option<bool>,
    pub actions: Option<Vec<A11yAction>>,
}
```

合并规则（与 `PaintSpec` / `StylePatch` 同构，「patch 覆盖 override」）：

```text
effective.role     = patch.role.or(override.role).unwrap_or(by_kind)
effective.label    = patch.label.or(override.label)
effective.value    = patch.value.or(override.value)  // 文本节点恒为 TextContent（§5.2）
effective.disabled = patch.disabled == Some(true) || override.disabled || style.interactive == Some(false)
effective.hidden   = patch.hidden == Some(true) || override.hidden
                     || style_patch.visible == Some(false)
                     || style.inner.display == Display::None
                     || (kind ∈ {Canvas, Geometry} && effective.label.is_none())   // §5.4
effective.selected = patch.selected
effective.checked  = patch.checked
effective.expanded = patch.expanded
effective.actions  = patch.actions.or(override.actions).unwrap_or(by_role)
                     ∪ {Focus}（若 focusable()）
```

**与 `StylePatch` 的一致性（硬规则）**：

| 引擎事实                                | a11y 表达                                          | 不写会怎样                                                                                    |
|-----------------------------------------|----------------------------------------------------|-----------------------------------------------------------------------------------------------|
| `StylePatch.visible == Some(false)`     | `hidden = true`                                    | AT 能读到"看不见的按钮"，与命中测试（`hit_test.rs:113-115`）不一致                            |
| `StylePatch.interactive == Some(false)` | `disabled = true`（**不是** hidden）               | `focusable()` 已排除它（`interaction/focus.rs:21-26`），AT 侧也必须显示为 disabled 而不是消失 |
| `Display::None`                         | `hidden = true`                                    | 同第一行（`hit_test.rs:110-112` 已排除）                                                      |
| `NodeKind::Canvas` 无替代文本           | `hidden = true` + `MissingAltText`                 | §7.4 明令"不静默丢弃"                                                                         |
| 焦点在某个 hidden 节点上                | **不改 hidden**（AccessKit 会强制 Include，F-110） | 与 AT 行为不一致地"藏掉焦点"                                                                  |

`A11yPatch` 的求值点见 §7.3（`refresh_styles` 内）。`A11yPatch` 变化 ⇒ `mark_a11y(node)`（**不产生 damage**）。

### 4.4 `A11yNodeId`（编码 / 代际 / 陈旧检测）

```rust
/// a11y 侧的节点身份。编码 = 引擎 `NodeId` 的位模式（`(generation << 32) | index`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct A11yNodeId(u64);

impl A11yNodeId {
    pub const fn from_engine(node: crate::node::id::NodeId) -> Self;  // node.to_bits()
    pub const fn get(self) -> u64;
    /// 解码回引擎句柄（代际 0 的编码非法 ⇒ `None`）。仅 crate 内可见（`NodeId::from_bits` 是 `pub(crate)`）。pub(crate) const fn from_bits(bits: u64) -> Self 是它的逆运算入口。
    pub(crate) const fn to_engine(self) -> Option<crate::node::id::NodeId>;
}
```

- **不需要 node→accesskit NodeId 的映射表**：`accesskit::NodeId(pub u64)` 的字段是 pub（`lib.rs:715`），
  引擎的 `NodeId::to_bits()` 已经是 `(generation << 32) | index`（`node/id.rs:38-40`，P04 起就是 taffy 的编码），
  代际为 0 永不出现（`node/id.rs:91-98`）⇒ **`accesskit::NodeId = A11yNodeId`，天然带代际**。
- **陈旧检测**：`A11yNodeId::to_engine()` → `Option<NodeId>` → `tree.get(node)` 失败即陈旧
  （probe ① #14 验证：同槽位下一代 id 查不到）。命中则忽略该 `ActionRequest` 并记
  `A11yDiagnostic::StaleAction`。
- `accesskit_windows` 的占位根是 `NodeId(0)`（`adapter.rs:374`），与本编码**不冲突**（生成式恒非 0）。
- `TreeId` 恒为 `TreeId::ROOT`（单根树，§1.2）。

### 4.5 `A11yTreeSnapshot`（进 `FrameRecord`）

```rust
/// 一次 a11y 交付（全树或增量）的只读结果。CPU 测试的断言面（§7.9 的落地形态）。
#[derive(Clone, Debug, PartialEq)]
pub struct A11yTreeSnapshot {
    /// 本次是否发出全树。
    pub full: bool,
    /// 本次发出的节点（`tree.order()` 序；全树时 = 全部 mounted 节点）。
    pub nodes: Vec<A11yNode>,
    /// 焦点（恒存在，见 §6.3）。
    pub focus: A11yNodeId,
    /// 本次交付触发的诊断条数（详情在 `App::a11y_diagnostics`）。
    pub diagnostics: usize,
}

impl A11yTreeSnapshot {
    pub fn nodes(&self) -> &[A11yNode];
    pub fn node(&self, id: A11yNodeId) -> Option<&A11yNode>;
    pub fn node_for(&self, engine: NodeId) -> Option<&A11yNode>;
    pub fn focus(&self) -> A11yNodeId;
    /// 焦点节点（恒 `Some`：回退链的终点是 root，root 恒在）。
    pub fn focus_node(&self) -> &A11yNode;
    /// 非 hidden 的节点（`hidden` 子树按**继承**语义排除，与 `set_hidden` 一致）。
    pub fn visible_nodes(&self) -> impl Iterator<Item=&A11yNode>;
    pub fn hidden_nodes(&self) -> impl Iterator<Item=&A11yNode>;
    pub fn children_of(&self, id: A11yNodeId) -> Vec<A11yNodeId>;
}

// `A11yPassReport` **不落地**：它没有非 test 消费者，计数直接读快照的
// `full` / `nodes.len()` / `diagnostics`（DEVIATIONS S-114）。
```

`A11yTreeReport`（`A11yBridge::take` 内部的候选集合，crate 私有）只承载 `{ full, ids: SmallVec<[NodeId; 16]> }`。

---

## 5. 节点 → a11y 映射

### 5.1 发出集合与判据顺序

**发出集合 = 全部 mounted 节点**（`tree.order()`，含 hidden 与 `Display::None`）。不按"相关性"裁剪，理由：

1. 增量成本：裁成"只发有语义的节点"后，任一子孙变化都要**重算最近已发祖先的 `children` 压缩表**
   （O(子树的子树)）；全发时 `children` 恒等于引擎 `children`，增量 O(1)。
2. 透明性已由消费端处理：`GenericContainer` 是 `ExcludeNode`，UIA 侧自动把它的子节点上提到祖父
   （probe ① #3/#4 实测）。**引擎不需要自己做压缩**。
3. 可达性最简单：任何节点都能不带父地单独更新（probe ① #8）。

**每个节点的判定顺序**（`a11y/mapping.rs`，纯函数，全部只读 `&Node` + `&NodeTree`）：

```text
1. role     ← patch.role ?? override.role ?? by_kind(node)            §4.3 / §5.2
2. hidden   ← ·patch.hidden ?? override.hidden ?? false
              || style_patch.visible == Some(false)
              || style.inner.display == Display::None
              || (kind ∈ {Canvas, Geometry} && label.is_none())        §4.3
3. disabled ← patch.disabled ?? override.disabled ?? false
              || style_patch.interactive == Some(false)                §4.3
4. label/value/description/selected/expanded ← 合并后的值                §4.3
5. actions  ← patch.actions ?? override.actions ?? by_role(role)
              ∪ {Focus} 若 focusable(tree, node)                        §5.3
6. bounds   ← physical(scroll::to_screen(tree, node, layout.bounds))    §5.5
7. children ← tree.children(node)（引擎直接子，未过滤）                  §5.1
8. parent   ← tree.parent(node)
```

### 5.2 逐节点种类表（`by_kind`）

| 引擎节点      | 判据（按序）                                    | `A11yRole`                                    | `label`                                               | `value`                                     | 默认 actions                                                    |
|---------------|-------------------------------------------------|-----------------------------------------------|-------------------------------------------------------|---------------------------------------------|-----------------------------------------------------------------|
| 窗口根        | `node == tree.root()`                           | `Window`                                      | `WindowOptions.title`（`Source::Override`：显式名字） | —                                           | —                                                               |
| 自绘          | `NodeKind::Canvas` / `Geometry`                 | `Image`                                       | **必填**（`A11yOverride.label`，`Source::Override`）  | —                                           | `Click`（若声明了 `on_click`）                                  |
| 文本          | `NodeKind::Text`                                | `Label`                                       | —                                                     | `TextContent.text`（`Source::TextContent`） | —                                                               |
| 滚动容器      | `scroll_slot(node).is_some()`                   | `ScrollView`                                  | —                                                     | —                                           | `ScrollUp/Down/Left/Right`、`ScrollIntoView`、`SetScrollOffset` |
| 虚拟列表      | `virtual_slot(node).is_some()`                  | `List`                                        | —                                                     | —                                           | —                                                               |
| 虚拟列表行    | 父有 `virtual_slot`                             | `ListBoxOption`                               | `A11yPatch.label`（应用给行文本）                     | —                                           | `Click`（若声明）                                               |
| 浮层          | `overlay_slot(node).is_some()`                  | `Dialog`（可用 override 改 `Menu`/`Tooltip`） | —                                                     | —                                           | `Expand`/`Collapse`（若声明）                                   |
| 可聚焦/可点击 | `focusable()` 或 `interaction.action.is_some()` | `Button`                                      | override / patch                                      | —                                           | `Click` + `Focus`                                               |
| 其余          | —                                               | `GenericContainer`                            | override / patch                                      | —                                           | —                                                               |

**不做名字推断**：`Button` 不会把子 `Text` 拼成名字（那会在增量下发时变成隐式依赖：任何子文本变化都要重发父）。
应用要名字就显式给 `A11yOverride.label` / `A11yPatch.label`；缺名字的交互节点记
`A11yDiagnostic::MissingLabel`（不隐藏、不 panic——AccessKit 允许无名控件）。

### 5.3 默认动作表（`by_role`）

| `A11yRole`                         | 默认 actions                                                    | accesskit 侧                                    |
|------------------------------------|-----------------------------------------------------------------|-------------------------------------------------|
| `Button`                           | `Click`                                                         | `add_action(Action::Click)`                     |
| `TextInput` / `MultilineTextInput` | `SetValue`、`SetTextSelection`                                  | 同名                                            |
| `ScrollView`                       | `ScrollUp/Down/Left/Right`、`ScrollIntoView`、`SetScrollOffset` | 同名（`ScrollIntoView` 只需 action，无需 data） |
| `List` / `ListBoxOption`           | （无）                                                          | —                                               |
| `Dialog` / `Menu`                  | `Expand`、`Collapse`                                            | 同名                                            |
| 其它                               | （无）                                                          | —                                               |

`Focus` 动作**不按角色给**，而是按 `interaction::focus::focusable(tree, node)`（`focus.rs:21-26`）——
与 Tab 序、键盘分发**同源**（P10 §6.2 的接口，正是为 P11 留的）。
`A11yPatch.selected == Some(v)` ⇒ `set_selected()`（`v == false` 时不置位，`Node` 的旗标只有 set/clear 语义）；
`effective.checked == Some(v)` ⇒ `set_toggled(Toggled::from(v))`（**显式 `false` 也写**：`accesskit_windows` 的
`is_selection_item_pattern_supported` 对 `RadioButton` 要求 `toggled()` 是 `Some(True|False)`，不写就没有
`SelectionItemPattern`；P15 F-151）；
`effective.disabled` ⇒ `set_disabled()`。**disabled 不删动作**——UIA 的 `IsEnabled` 由 `Disabled` 决定，
删掉动作会让 AT 认为控件类型变了。

### 5.4 `Canvas` / `Geometry` 的替代文本与诊断

```text
if kind ∈ {Canvas, Geometry}:
    let Some(label) = effective.label else {
        hidden = true;                                  // §7.4「无替代文本时标 hidden」
        diagnostics.push(A11yDiagnostic::MissingAltText { node, role });   // 「不静默丢弃」
        // 仍然发出节点（hidden）——AT 侧不可见，但诊断/测试能查到它
    }
```

诊断形态对齐 `TextDiagnostic::FontStackMiss`（`text.rs` 的既有形态：结构化字段、无字符串泛化）：
`MissingAltText { node: NodeId, kind: Canvas | Geometry }`。`App::a11y_diagnostics()` 返回切片（与
`App::text_diagnostics()` 同型），容量上限 64（超出丢弃最旧，不增长）。

为什么 `Image` 而不是 `GraphicsObject`（F-109）：ARIA 的 `graphics-object` 必须位于 `graphics-document`
之内；单独一个 `Image` 在 UIA 映射为 `UIA_ImageControlTypeId`、在 NSAccessibility 映射为 `image` 角色，
两处都是"有名字的图形"的语义，正是自绘节点需要的。

### 5.5 几何换算（`bounds = 逻辑 × scale`）

```rust
// a11y/mapping.rs
pub(crate) fn bounds_of(tree: &NodeTree, node: NodeId, scale: Scale) -> PhysicalRect {
    let content = tree.get(node).map_or(Rect::default(), |n| n.layout().bounds);
    let screen = crate::interaction::scroll::to_screen(tree, node, content);  // 减**祖先**滚动偏移（P10 S-99）
    screen.to_physical(scale)                                                // 逐边 round（P01 §7.3）
}
```

- **窗口相对**，不是屏幕绝对坐标：`accesskit::Node::bounds` 的契约是"相对 tree 容器（窗口）原点、物理像素、
  y 向下"（`lib.rs:2520-2522`）。
- 用 `to_physical`（逐边 round）而**不是** `to_physical_covering`（向外 floor/ceil）：后者是 damage/AA 的安全侧
  （P01 §7.4），给 a11y 会把每个盒子虚胖 1 px，AT 的点击命中会偏。
- 与 `Probe::bounds` 同源（都是 `Node.layout().bounds`），差别只有滚动偏移与 ×scale ⇒ §10.4 的
  `bounds == Probe × scale` 断言是**同一份几何**，满足 §7.8「视觉命中与 a11y 命中一致」。
- **scale 变化**：`Window::set_scale` 让 `A11yBridge.bounds_epoch += 1`；下一 pass 见 epoch 变化 ⇒ 发全树
  （bounds 全变，增量没有意义）。不改 `Node` 的任何脏位。

### 5.6 虚拟列表 / 浮层 / 滚动（与 P10 的接口）

| 场景           | a11y 表现                                                                                                                                           | P10 已就绪的接口                                                                                                                     |
|----------------|-----------------------------------------------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------|
| 虚拟列表       | 只有**已挂载**的行出现在 `children`（未挂载的行不存在）；行数变化 ⇒ 容器 `mark_structure` ⇒ 容器重发                                                | `VirtualListState { count, first, last, mounted, .. }`（`interaction.rs:305-317`，注释已写明"也供 a11y 计算 pos_in_set / set_size"） |
| 滚动           | 容器的 `children` 不变，但子节点的 `bounds` 随偏移变化 ⇒ 容器（含子树）**必须重发**：`App::scroll_to` 成功后 `bridge.invalidate_subtree(container)` | `ScrollHandle` / `ScrollState` / `scroll::to_screen`                                                                                 |
| 浮层           | 浮层是 root 的**末尾子节点**（P10 S-89）⇒ a11y `children` 顺序天然与绘制序一致；`Dialog`/`Menu` 角色由 override 给                                  | `overlay_slot` / `Overlays`                                                                                                          |
| 虚拟列表行回收 | 行节点被 `remove` ⇒ 父（容器）`mark_structure` ⇒ 重发容器的 `children`（probe ① #10 的删除路径）；**若被回收的正是焦点行**，焦点回退到容器（§6.3）  | `rewindow_virtual`                                                                                                                   |

> `pos_in_set` / `set_size`（`Node::pos_in_set` / `set_size_of_set`）v1 **不填**：它们要求 `List` 有稳定条目
> 语义，而 `virtual_list` 只保证"已挂载窗口"。记录在 V-78（不阻塞）。

---

## 6. 增量算法

### 6.1 变更集合

pending 由三类来源组成（`a11y/adapter.rs` 的 `A11yBridge`）：

```text
① full          —— 首次交付 / scale 变化（bounds_epoch）/ 捕获开启 / 祖先链断裂后的重同步
② nodes         —— tree.order() 中带 (A11Y | ORDER) 的节点        （A11Y 见 §6.2 的写点表）
                    + 每个 pending 节点的**全部祖先**（去重；循环到 root）
③ subtrees      —— 滚动容器：`invalidate_subtree(container)`（内容整体平移，子孙 bounds 全变）
```

- **祖先链无条件带上**（不是"只对新节点"）：消费端对"数据未变的节点"不产生 `updated` 事件
  （`tree.rs:271` 的 `if node_state.data != node_data`），所以多带祖先只花构造/序列化的带宽；
  好处是**任何**结构增删（新节点、回收行、重排）都自然满足"新节点必须有父"（probe ① #9）。
  典型深度 5–15，去重用线性 `SmallVec::contains`（pending 规模通常 < 32）。
- **ORDER**：`mark_structure` 置 `LAYOUT|PAINT|ORDER|A11Y`；a11y pass 读 `A11Y | ORDER` 并**两者都清**
  ⇒ P10 的 V-65（「`ORDER` 至今没有清除者」）在 P11 关闭。
- **卸载**：不需要单独的"删除集合"——`tree.remove` 里 `mark_structure(parent)`（`tree.rs:663`）已经让父进
  pending，父的新 `children` 少了被删子树 ⇒ 消费端整棵回收（probe ① #10）。**被删节点绝不出现在 `nodes` 里**
  （设计原文 §7.5 的"被删节点不再出现"）。

### 6.2 `A11Y` 的写点表（谁标它）

| 写点           | 代码位置                                                                             | 语义                        |
|----------------|--------------------------------------------------------------------------------------|-----------------------------|
| 节点创建       | `insert_child` 的初值 `LAYOUT｜PAINT｜A11Y`（`tree.rs:724`）                         | 新节点必须发                |
| 结构变化       | `mark_structure(node)`（`insert_child` / `remove` 里对父调用）                       | `children` 数组变了         |
| 动态属性       | `refresh_styles` 在 `patch.interactive`/`patch.visible` 变化时（`app.rs:1957-1967`） | 视觉/可交互性变了           |
| 动态 a11y 补丁 | `refresh_styles` 在 `A11yPatch` 变化时（**P11 新增**，§7.3）                         | label/value/disabled/… 变了 |
| **布局**       | `mark_layout` 的集合加 `A11Y`（**P11 改**，`dirty.rs` / `tree.rs:818`）              | bounds 变了 ⇒ 必须重发      |
| 焦点           | `App::set_focus` 对旧/新焦点 `mark_a11y`（P10 已有，`app.rs:1093`）                  | 焦点节点身份要重发          |
| 滚动           | `A11yBridge::invalidate_subtree(container)`（**P11 新增**，不走脏位）                | 子孙 bounds 整体平移        |

`mark_layout ⊇ A11Y` 的**理由**：`settle_frame` 只清 `LAYOUT|PAINT`（`tree.rs:950-959`），a11y pass 跑在帧之后
（§8.2）——如果 `A11Y` 不由 `mark_layout` 置位，bounds 变化在 pass 看来就消失了。`A11Y` 不清 ⇒ 累积 ⇒
"自上次交付以来发生过布局"的**精确**集合，正是增量需要的集合。

`A11Y` **不产生 damage、不清 `REUSABLE`**：`NodeTree::touch` 只在 flags 含 `LAYOUT|PAINT` 时
`accumulate_damage`（`tree.rs:1604-1614`，DEVIATIONS A-N3 的既有语义），P11 不改它。

### 6.3 焦点：每帧必带 + 回退

```text
let raw = window.focus().focused();          // 单一权威源（P10 §6.1）
let focus = match raw {
    Some(node) if emitted.contains(node) => A11yNodeId::from_engine(node),
    Some(node) => {                           // 未发出（虚拟列表窗口外的行 / 已卸载但焦点未清理）
        diagnostics.push(A11yDiagnostic::MissingFocusTarget { node });
        nearest_emitted_ancestor(node).map(A11yNodeId::from_engine).unwrap_or(root_id)
    }
    None => root_id,                          // §7.8「None ⇒ 退化到窗口根」
};
```

- **`TreeUpdate.focus` 每次都带**（`lib.rs:3252-3261` 的强制要求；probe ① #11 证明 `nodes: []` + `focus` 合法）。
- **回退的真实理由不是 hidden**：focused 的 hidden 节点被消费端强制 `Include`（`filters.rs:17-24`，probe ① #13）。
  会 `panic` 的是"focus 不在 `nodes` 里"（`tree.rs:86-96`，probe ① #12）——即**未发出的节点**。
  `nearest_emitted_ancestor` 只在"行被回收但 `FocusScope` 还指着它"时命中。
- `update_window_focus_state(bool)` 与 `TreeUpdate.focus` 是**两件事**：前者是"窗口有没有键盘焦点"（决定
  `is_host_focused`，`tree.rs:534-541`），后者是"树内谁是焦点"。窗口失焦时**不改** `focus`（保留原值），
  否则重新聚焦会丢焦点。

### 6.4 结构变化：父 `children` 重发

| 变化                   | pending 内容                                                                                    | 消费端结果                               |
|------------------------|-------------------------------------------------------------------------------------------------|------------------------------------------|
| 挂新子节点             | 父（`mark_structure`）+ 新节点（初值 A11Y）+ 祖先                                               | 父的 `children` 变长；子被 `add_node`    |
| 卸载子树               | 父（`mark_structure`）                                                                          | 父的 `children` 变短；被删子树 `removed` |
| 重挂（`append_child`） | 旧父 + 新父（两处 `mark_structure`）+ 被移动子树（`mark_subtree_paint`，含 A11Y? **否**——见下） | 两个父的 `children` 都更新               |
| 顺序变化               | 同"重挂"（`order_insert`/`order_remove_subtree` 后父被标）                                      | `children` 顺序 = `tree.order()` 的先序  |

⚠️ **P11 必须补的一条**：`append_child` 的 `mark_subtree_paint`（`tree.rs:1648-1653`）只标 `PAINT`。
重挂后子树内节点的 **`parent`** 变了（a11y 的 `parent` 字段与 `bounds` 都可能变）⇒ P11 在该函数里追加
`A11Y`（仍是"不产生 damage"的位）。这是 §7.5「结构变化重发父 children」之外唯一需要动的脏位写点。

### 6.5 首次激活（全树）与懒激活（零开销）

```text
// A11yBridge
full_pending: bool   // 初值 true；`set_scale` ⇒ true；`capture` 打开 ⇒ true
                     // 由 build_update 置 false（**只在真正产出了 TreeUpdate 时**，即闭包被执行时）
```

- 首次：`ActivationHandler::request_initial_tree()` 恒返回 **`None`**（`platform/windows/a11y.rs` 的
  `WindowsActivation`），AccessKit 用 `PLACEHOLDER_ROOT_ID` 顶上（`adapter.rs:564-578`），随后
  `handle_wm_getobject` 之后主线程的第一次 `update_if_active` 里闭包执行 ⇒ 我们发全树
  （`full_pending == true`）。probe ② #5/#6 实测这条路径。
- **为什么不用 ActivationHandler 直接返回全树**：`request_initial_tree` 在 `WM_GETOBJECT` 里被调用，
  而 `WndProc` **不碰 `App`**（P09 §5.2 的纪律，`event_loop.rs:10-11` 的模块注释）。要在那里返回全树，
  就得把整棵 `TreeUpdate` 缓存在平台层——多一份真值、多一次全树克隆，且与 §8.2 的"每轮调用"重复。
  占位一帧（< 16 ms）是 AccessKit 明确支持的用法（`lib.rs:3351-3358`）。
- **懒激活**：`update_if_active` 在 `Inactive` 时**不执行闭包**（`adapter.rs:467-468`，probe ② #2/#4）⇒
  没有任何 a11y 构造开销。但平台层**每轮**仍要调用它（O(1) 的 `match`）——这是能够"从 Inactive 变 Active
  的那一刻就被发现"的唯一方式。
- **`update_if_active` 的调用条件**：`if app.a11y_pending(window) { native.update_a11y(&mut app) }`，
  其中 `a11y_pending` 是 O(1)（`tree.a11y_dirty() > 0 || bridge.full_pending || bridge.subtree_pending()`）。 **不能**只用
  `update_if_active` 而不判 pending：那样 adapter 一旦 Active，每轮都要执行闭包（构全树判断）。

### 6.6 与 damage / 帧门禁的关系

| 事实                                                 | 后果                                                                                           |
|------------------------------------------------------|------------------------------------------------------------------------------------------------|
| `A11Y` 不在 `touch` 的 damage 分支里                 | a11y 更新**不产生** `DamageRect`（§7.5 末行）                                                  |
| `needs_frame` 只看 `LAYOUT                           | PAINT                                                                                          |!REUSABLE|wake|damage`（`window.rs:378-386`） | 纯 A11Y 变化 ⇒ 帧空闲 ⇒ `FrameOutcome::Idle`（P06 §9.3 的第二段门禁） |
| 因此 a11y pass 不能挂在 `App::frame` 的 pass 链里    | 它由**平台泵每轮**调用（`pump` 的第 ④′ 步）+ 测试捕获路径调用（`App::frame` 开头，§10.2）      |
| 纯 a11y 变化导致 `Idle` 时 `settle_frame` **不执行** | `LAYOUT` 不会被清 ⇒ a11y pass 仍能看到它们（这也顺带说明为什么 `A11Y` 可以安全地累积到下一轮） |

⚠️ 注意一个**既有耦合**：`App::frame` 在 `Idle` 分支（`!needs_frame`）直接 `record_idle` 返回，**一个 pass 都不跑**。
若某帧只有 `A11Y` 变化，则该帧的 `refresh_styles` 也不会跑 ⇒ `bind_a11y` 的依赖重算被推迟到下一帧有像素时。
这是**可接受的**：a11y 值只在交付时才被读到（`build_update`），而交付本身由 pending 驱动；`bind_a11y`
的求值时机与 `bind_style` 完全一致（同一 pass），不引入新的时序例外。

### 6.7 伪代码（`a11y/adapter.rs`）

```text
A11yBridge::build_update(tree, window, scale) -> TreeUpdate:
    full = self.full_pending || self.bounds_epoch != self.seen_bounds_epoch
    ids  = if full { tree.order().to_vec() }
           else {
               let mut set = self.collect_dirty(tree)          // A11Y|ORDER，同时清位
               self.add_subtrees(&mut set, tree)               // 滚动容器整棵子树
               self.add_ancestors(&mut set, tree)              // 每个 id 的祖先链（去重）
               set
           }
    let nodes: Vec<(NodeId, Node)> = ids.iter()
        .filter_map(|id| A11yNode::build(tree, *id, scale, style_patch, a11y_patch))  // §5.1 的 8 步
        .map(|n| (n.id.accesskit(), n.to_accesskit()))                                   // §5.2/§5.4
        .collect();
    let focus = self.resolve_focus(tree);                       // §6.3
    let tree_info = full.then(|| TreeInfo { root: root.into(), toolkit_name: "Kytrix",
                                            toolkit_version: env!("CARGO_PKG_VERSION") });
    self.full_pending = false;
    self.seen_bounds_epoch = self.bounds_epoch;
    TreeUpdate { nodes, tree: tree_info, tree_id: TreeId::ROOT, focus }
```

`A11yNode::to_accesskit()`（`a11y/mapping.rs`）：

```text
node = Node::new(role_to_accesskit(self.role));
if !children.is_empty() { node.set_children(children.iter().map(|c| c.accesskit()).collect()) }
if hidden   { node.set_hidden() }
if disabled { node.set_disabled() }
if let Some(sel) = selected { node.set_selected() / clear（false 时不置位） }   // 见下
if let Some(checked) = checked { node.set_toggled(Toggled::from(checked)) }      // 显式 false 也写（F-151）
match label_source {
    Override    => node.set_label(text),
    TextContent => node.set_value(text),
}
if value.is_some() && label_source != TextContent { node.set_value(value) }
if let Some(desc) = description { node.set_description(desc) }
for action in actions { node.add_action(action_to_accesskit(action)) }
node.set_bounds(Rect::new(b.x0 as f64, b.y0 as f64, b.x1 as f64, b.y1 as f64))
```

`selected == Some(false)` 不置位（`set_selected` 只表达 true；`Node` 没有"显式 false"的语义——`Flag` 位只有
set/clear，clear 在"重发未设置"时等价于 false，符合"更新即全量覆盖该节点"的契约，`lib.rs:3213-3218`）。

---

## 7. `A11yBridge` / `A11yTree` / `NodeA11y`

### 7.1 状态归属（唯一真值）

| 状态                                                                       | 归属                                                                 | 理由                                                                                                       |
|----------------------------------------------------------------------------|----------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------|
| a11y 静态声明（`A11yOverride`）、动态补丁（`A11yPatch`）、`bind_a11y` 闭包 | `NodeTree.a11y_slots`：`Vec<Option<Box<NodeA11y>>>`（按 arena 槽位） | 与 `layout_cache` / `clip_keys` / `text_slots` / `debug_ids` 同型（S-42 / S-66）；`Node` 不加字段（§11.1） |
| pending 集合 / `full_pending` / `bounds_epoch` / `capture` / 最近快照      | `Window.a11y: A11yBridge`                                            | 每窗口一份；`paint`/`App` 都只有 `&Window`                                                                 |
| 焦点                                                                       | `Window.focus: FocusScope`（P10，**不复制**）                        | §7.8「单一权威焦点源」                                                                                     |
| Windows `Adapter` / `WindowsActivation` / `WindowsActionHandler`           | `platform/windows/a11y.rs`（挂在 `WndProcState`）                    | 平台类型不得进共享层（I9）                                                                                 |
| 应用 a11y 动作 handler 表 / 诊断                                           | `App`                                                                | 与 `on_action` / `text_diagnostics` 同型                                                                   |

```rust
// a11y/tree.rs —— 侧表载荷
pub(crate) struct NodeA11y {
    pub(crate) over: Option<A11yOverride>,
    pub(crate) bind: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> A11yPatch>>,
    pub(crate) patch: A11yPatch,   // 上次求值结果（diff 用）
}
```

`NodeTree` 新增（全部 `pub(crate)`，**每个类型只有一个 inherent `impl` 块**——写进已有的 `impl NodeTree`）：

```rust
fn a11y_of(&self, node: NodeId) -> Option<&NodeA11y>;
fn take_bind_a11y(&mut self, node: NodeId) -> Option<Box<dyn Fn(&mut ReadCtx<'_>) -> A11yPatch>>;
fn put_bind_a11y(&mut self, node: NodeId, bind: Box<dyn Fn(&mut ReadCtx<'_>) -> A11yPatch>) -> bool;
fn set_a11y_patch(&mut self, node: NodeId, patch: A11yPatch) -> bool;
fn clear_a11y(&mut self, node: NodeId) -> bool;      // 清 A11Y 位 + a11y_dirty -= 1
fn a11y_dirty(&self) -> u32;                         // O(1) 计数（`touch` 里只在 A11Y 由无到有时 +1）
fn mark_a11y_subtree(&mut self, node: NodeId) -> usize; // 重挂/滚动子树用（迭代 DFS，不递归）
```

侧表的生命周期与 `clip_keys` 完全一致：`insert_child` 写 `spec.a11y`、槽位复用置 `None`（`tree.rs:744-747` 同款），
`remove` 逐槽 `None`（`tree.rs:641-657` 同款）。

### 7.2 入口：`build_update` 在哪里被调用

| 调用方                                                                                | 目的                                                                                            |
|---------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------|
| `NativeWindow::update_a11y(&mut self, app: &mut App)`（`platform/windows/window.rs`） | 生产路径：在 `delta.update_if_active(                                                           || a11y::adapter::build_update(app, id, ..))` 的闭包里调用 ⇒ **闭包不执行就没有任何构造** |
| `TestApp::drain`（`#[cfg(feature = "test-support")]`）                                | 测试捕获：**帧之后** `a11y_take` + `attach_a11y` 写进 `FrameRecord.a11y`（§10.1，与平台泵同序） |

`build_update` 的签名（crate 私有）：

```rust
pub(crate) fn build_update(app: &mut App, window: WindowId) -> accesskit::TreeUpdate;
```

它做四件事：①从 `App` 取出该窗口的 `tree` / `focus` / `scale`；②`A11yBridge` 收集 pending 并清位；③构造
`A11yNode` 列表；④转 `TreeUpdate`。**不做**平台调用、不发事件（`raise()` 在平台层）。

> 捕获路径**不**清 `full_pending`、也不与 adapter 争 pending：`capture` 只在测试 harness 里打开
> （`TestApp` 从不创建原生窗口），生产构建里 `capture == false` ⇒ 只有一个消费者（adapter）。这是
> "同一时刻只有一个 drainer"的**可断言**规则（§10.5 的用例 ①）。

### 7.3 `bind_a11y` 的求值点（`refresh_styles` 内，与 `bind_style` 同批）

`App::refresh_styles` 现在对 `PAINT` 脏节点取 `bind_style` / `bind_text` 两个闭包、用**同一个** `ReadTracker`
求值、`replace_deps(owner, old, new)` 之后 `set_deps`（`app.rs:1913-1997`）。P11 加第三个：

```rust
let bind_a11y = tree.take_bind_a11y(node);
// … 同一个 ReadCtx、同一个 tracker …
let a11y_patch = bind_a11y.as_ref().map_or_else(A11yPatch::default , | f| f( & mut ctx));
// … put_bind_a11y / set_a11y_patch …
if a11y_patch != old_a11y_patch {
tree.set_a11y_patch(node, a11y_patch);
tree.mark_a11y(node);
}
```

**为什么不做独立 pass**（这是本阶段最容易做错的点）：

1. **依赖集是每节点一份**：`DependencyGraph::replace_deps(owner, old, new)` 整体替换 `Owner::Node{..}` 的边集
   （`dependency.rs:55-82`）。若 `bind_a11y` 在另一个 pass 里求值，它会用**只有自己读过的**信号集合覆盖
   `bind_style` 的边集 ⇒ `bind_style` 的依赖悄悄丢失（写该信号不再唤醒）。 **一条 pass、一个 tracker、一份边集**是唯一自洽的形态。
2. 求值对象只有声明了 `bind_a11y` 的节点（`take_bind_a11y` 返回 `None` ⇒ 零成本），与 `bind_text` 完全同构。
3. `A11yPatch` 的 diff 只在"补丁真的变了"时标 `A11Y`（`PartialEq`，值语义），幂等。

**声明位置**：`NodeBuilder::a11y(A11yOverride)`（静态）+ `NodeBuilder::bind_a11y(impl Fn(&mut ReadCtx) -> A11yPatch)`
（动态）。两者可同时用（静态兜底 + 动态覆盖），也可都不用（纯推导，§5.2）。

### 7.4 `A11Y` 与 `PAINT` 的关系（`bind_a11y` 禁用态不产生视觉 damage）

P03 §17.9 的用例 ⑪ 已定：`interactive` 变化只标 `A11Y`（不标 `LAYOUT`/`PAINT`）。P11 的对应断言：
`bind_a11y` 返回 `A11yPatch { disabled: Some(true) }` ⇒ 节点含 `A11Y`、**不含** `LAYOUT`/`PAINT`、
`Node.damage` 不变、`FrameOutcome::Idle`（§7.9 的「`bind_a11y` 正确性」行）。实现上不需要新代码：
`mark_a11y` 不碰 damage（`tree.rs:828-830` → `touch(A11Y)` 不命中 `LAYOUT|PAINT` 分支）。

### 7.5 错误与诊断

| 情况                                                         | 处理                                                                 | 类型                                   |
|--------------------------------------------------------------|----------------------------------------------------------------------|----------------------------------------|
| adapter 构造失败（窗口已可见 / `Adapter::new` 前后状态异常） | `EngineError::A11yInit { detail }`，窗口创建失败（首显前，不进入泵） | `error.rs`（phase-01 §6.2.2 变体 #17） |
| `Canvas`/`Geometry` 缺替代文本                               | 节点 `hidden` + `A11yDiagnostic::MissingAltText`                     | 不 panic、不静默                       |
| 交互节点无名字                                               | `A11yDiagnostic::MissingLabel`                                       | 同上                                   |
| 焦点指向未发出的节点                                         | 回退最近已发祖先 + `MissingFocusTarget`                              | 不 panic                               |
| `ActionRequest` 的目标陈旧/不存在                            | 忽略 + `StaleAction`                                                 | 不 panic（§9.2）                       |
| `ActionRequest` 的动作未映射/无 handler                      | 忽略 + `UnhandledAction`                                             | 不 panic                               |

诊断的**容量与读取**：`App::a11y_diagnostics(&self) -> &[A11yDiagnostic]`，内部 `Vec` 上限 64（`text_diagnostics`
同款，超出丢弃最旧）。`A11yTreeSnapshot.diagnostics` 只给条数（快照必须轻）。

---

## 8. Windows adapter（`platform/windows/a11y.rs`）

### 8.1 构造时机与 `Adapter` / `SubclassingAdapter` 的选择

**选 `Adapter`（自建 WndProc 直连），不用 `SubclassingAdapter`**：

| 理由                                                                                   | 说明                                                                                                                       |
|----------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------|
| 我们**已经有**自己的 `WndProc`                                                         | `RegisterClassW(lpfnWndProc: Some(wndproc))`（`window.rs:288-297`）；再叠一层 `SetWindowLongPtrW` 子类化是第二套消息分发链 |
| 子类化会把 4 个焦点消息**吞掉**                                                        | `subclass.rs:62-67` 在自己的 `wnd_proc` 里处理 `WM_SETFOCUS`/`WM_KILLFOCUS`/`WM_ENTER(SIZE                                 |MENU)LOOP`，我们的 `translate` 也在处理它们 ⇒ 两处都需要，链式调用顺序成为隐患 |
| `SubclassingAdapter` 无 `update_window_focus_state` / `handle_wm_getobject` 的公开入口 | 它自己全包；我们的 `WndProcState` 需要显式控制调用时机（借用释放纪律，§8.2）                                               |
| `SubclassingAdapter::new` 在窗口可见时 **panic**（`subclass.rs:167-171`）              | 我们的构造点在 `NativeWindow::create`（隐藏），本身满足；但 panic 面是多余的失败模式                                       |

**构造时机：`NativeWindow::create` 内、`ShowWindow` 之前**（`window.rs:90-102` 的位置，紧跟
`SetWindowLongPtrW(GWLP_USERDATA)` 之后）：

```rust
// platform/windows/window.rs::create —— 在 window.apply_dpi_and_size(..)? 之前
let a11y = WindowsA11y::new(hwnd, dispatcher.clone(), id) ?;   // → EngineError::A11yInit
// 写入 WndProcState（state 已 `Box::into_raw`，此时独占）
```

- 必须在 `WM_GETOBJECT` 之前完成 `init_uia()`：`Adapter::new` 内部调它（`adapter.rs:441`），文档明确
  "不得在处理 `WM_GETOBJECT` 时构造"（`adapter.rs:416-422`）。放在 `create` 里满足（probe ② #1）。
- `is_window_focused` 传 **`false`**：新窗口还没显示，必然没有焦点；随后由 `WM_SETFOCUS` 纠正
  （`update_window_focus_state(true)`）。
- 失败：`Adapter::new` 不返回 `Result`（infra 初始化失败只会 panic/静默）。可失败的只有我们自己的包装
  （如 `Dispatcher` 缺失），因此 `A11yInit` 的**唯一生产者**是"窗口句柄无效 / 重复包装"这类前置校验；
  `Adapter::new` 之后不检查。

### 8.2 `WM_GETOBJECT` 在 `WndProc` 里的分发

`event_loop.rs::wndproc` 新增一个分支（放在 `state.is_null()` 检查**之后**、`WM_APP_FRAME_WAKE` 之前）：

```rust
if msg == WM_GETOBJECT {
// SAFETY: 与既有分支同口径——`state` 由 `SetWindowLongPtrW(GWLP_USERDATA)` 写入、`Drop` 前有效。
let state = & mut * state;
if state.a11y.in_getobject.get() {
// 重入防御：UiaReturnRawElementProvider 会同步派发嵌套 WM_GETOBJECT（adapter.rs:530-533）。
// 返回 0 = 本次不接管，UIA 会在外层返回后重试。
return LRESULT(0);
}
state.a11y.in_getobject.set(true);
// 借用作用域**只**覆盖 handle_wm_getobject；转换在作用域之外（照抄 subclass.rs:50-60 的纪律）。
let result = {
let a11y = & mut state.a11y;
let (adapter, activation) = ( & mut a11y.adapter, & mut a11y.activation);
adapter.handle_wm_getobject(wparam, lparam, activation)
};
state.a11y.in_getobject.set(false);
return match result {
Some(value) => LRESULT::from(value.into()),
None => DefWindowProcW(hwnd, msg, wparam, lparam),   // objid 不归我们管
};
}
```

要点：

1. **三参数**（`wparam`, `lparam`, `&mut activation`），与附录 A.3 / probe ② #5 一致。
2. **`None` ⇒ 必须交回 `DefWindowProcW`**（`adapter.rs:527-529` 的设计意图：非 client/root 的 MSAA objid）。
3. **借用释放纪律**：`handle_wm_getobject` 的返回值类型带 `use<H>`（`adapter.rs:539`），但实际是**自有值**
   （`WmGetObjectResult`）——照 `subclass.rs:50-60` 先释放再 `.into()`，避免 `UiaReturnRawElementProvider`
   派发的嵌套消息撞上未释放的借用。
4. **重入防御**：`in_getobject: Cell<bool>` 一个字段。AccessKit 自己靠"先 drop `RefMut`"躲开；我们用 `Cell`
   把"真的重入"变成显式 `LRESULT(0)` 而不是 UB（两层保险，成本 1 B + 一次读写）。
5. **`wndproc` 仍不碰 `App`**（P09 §5.2 / P10 S-93 的纪律）：`WM_GETOBJECT` 分支只碰 `WndProcState.a11y`。

窗口焦点同步（同样在 `wndproc`）：

```rust
if matches!(msg, WM_SETFOCUS | WM_EXITMENULOOP | WM_EXITSIZEMOVE) { state.a11y.focus(true); }
if matches!(msg, WM_KILLFOCUS | WM_ENTERMENULOOP | WM_ENTERSIZEMOVE) { state.a11y.focus(false); }
```

`WindowsA11y::focus(bool)`：`if let Some(events) = adapter.update_window_focus_state(focused) { events.raise() }`
——与 `subclass.rs:118-124` 逐字同构。**焦点消息已在 `translate` 里产生 `PlatformEvent::Focus`**，两条路互不依赖：
`PlatformEvent::Focus` 走 `App`（键盘分发/焦点环），`update_window_focus_state` 走 adapter（AT 的窗口焦点态）。

### 8.3 update 路径与 `raise()`

```rust
// platform/windows/window.rs
pub(crate) fn update_a11y(&mut self, app: &mut App) {
    let id = self.id;
    let state = self.state_mut();
    // 先判 pending（O(1)），避免 adapter 一旦 Active 就每轮构全树判断（§6.5）
    if !app.a11y_pending(id) { return; }
    let events = state.a11y.adapter.update_if_active(|| {
        crate::a11y::adapter::build_update(app, id)
    });
    // raise 必须在窗口线程、且不得持有任何锁（adapter.rs:633-642）
    if let Some(events) = events { events.raise(); }
}
```

- `QueuedEvents::raise` 的文档要求：**窗口线程**调用、且调用时**不得持有 `WM_GETOBJECT` 需要的锁**
  （`adapter.rs:633-642`）。泵在主线程、`update_a11y` 不持有任何外部锁 ⇒ 满足。
- `raise()` 期间可能收到 `WM_GETOBJECT`（嵌套消息）⇒ `WndProc` 会跑 → 那时 `adapter` 的借用已经结束
  （`update_if_active` 返回后不再借用）。
- 调用频次：**每轮泵迭代一次**（不是每帧）。`pump` 的 `④` 之后加 `④′`：

```text
④ app.run_frame_hook(); if dispatcher.take_frame_wake() { frame_all(&mut app)?; }
④′ for native in windows.iter_mut() { native.update_a11y(&mut app); }   // 纯 A11Y 变化只在这里发出
```

### 8.4 Action handler 的线程约束与回投

`Adapter::new` 要求 `impl 'static + ActionHandler + Send`（`adapter.rs:426`），且文档明确
"may or may not be called on the thread that owns the window"（`adapter.rs:413-414`）。
⇒ handler **只做一件事：入队**。

```rust
// platform/windows/a11y.rs
pub(crate) struct WindowsActionHandler {
    dispatcher: Arc<Dispatcher>,
    window: WindowId,
}

impl accesskit::ActionHandler for WindowsActionHandler {
    fn do_action(&mut self, request: ActionRequest) {
        // `Dispatcher::push` 自带 wake（合并位，P09 F-87）；不在 UIA 线程做任何 UI 工作。
        self.dispatcher.push(UserTask::new(move |app| {
            crate::a11y::action::dispatch(app, window, request);
        }));
    }
}
```

- `UserTask` 的契约要求 `FnOnce(&mut App) + Send`（`dispatcher.rs:19-30`）；`ActionRequest` 全部字段 `Send`
  （`NodeId(u64)` / `TreeId(Uuid)` / `Action`(Copy) / `Option<ActionData>`(Box<str>/f64/Point)）。
- **禁止**在 handler 里碰 `NodeTree`（§7.7 的禁令）：执行点只有主线程的 `dispatcher.drain_tasks()`（`pump` 第 ⑤ 步）。
- Windows 的 `ActionHandlerWrapper`（`context.rs`）会用 `Mutex` 包一层；我们的 handler 本身无状态（两个 Copy 字段）
  且线程安全。

### 8.5 关闭与 deactivate

```text
关闭顺序（pump 的「③ 关闭」分支，close_requested_windows）：
  1. app.close_window(id)                // 卸载节点树 + 关依赖边
  2. native.destroy()                    // DestroyWindow：此后不会再有 WM_*
  3. drop(WindowsA11y)                   // Adapter 随之 drop（无 deactivate API，F-111）
  4. NativeWindow::drop → Box::from_raw(state)（既有路径）
```

- **必须先 `DestroyWindow` 再 drop `Adapter`**：`Adapter` 内部持有 `HWND` 与 `Context`（COM 引用）。
  反序（先 drop）后窗口仍可能收到 `WM_GETOBJECT`，而 `WndProcState` 里已经没有 adapter（`None` 分支 → `DefWindowProcW`，
  也安全，但会让 UIA 拿到"窗口不实现 UIA"的结论，对后续重建窗口不利）。
- `DeactivationHandler::deactivate_accessibility` **不实现也不调用**：Windows adapter 从不调它（F-111）。
  P12 的 `accesskit_unix` 需要它（`adapter.rs:133` 的 `new` 三参数）——这是 §12 交接项之一。

### 8.6 `unsafe` 清单与 `SAFETY` 要求

`platform/windows/a11y.rs` 只允许以下几处 `unsafe`，每处必须有 `// SAFETY:`：

| 位置                          | 为什么                                                                                         | SAFETY 要点                                               |
|-------------------------------|------------------------------------------------------------------------------------------------|-----------------------------------------------------------|
| `Adapter::new(hwnd, ..)`      | `HWND` 是裸句柄                                                                                | 句柄来自本进程 `CreateWindowExW`，`create` 内独占，未显示 |
| `LRESULT::from(value.into())` | 内部 `UiaReturnRawElementProvider`（`adapter.rs:624`）                                         | 由 adapter 保证；外层不持有 `&mut`                        |
| 无其它                        | `handle_wm_getobject` / `update_if_active` / `update_window_focus_state` / `raise` 都是安全 fn | —                                                         |

`platform/windows/{event_loop,window}.rs` 新增的两处（`&mut *state`）沿用既有 `SAFETY` 文案（同一指针来源）。

---

## 9. Action 分发（`a11y/action.rs`）

### 9.1 `ActionRequest` → 引擎动作

```rust
pub(crate) fn dispatch(app: &mut App, window: WindowId, request: ActionRequest) {
    if request.target_tree != TreeId::ROOT { return; }                       // 单根树（§1.2）
    let Some(engine) = A11yNodeId::from_bits(request.target_node.0).to_engine() else {
        app.a11y_note(A11yDiagnostic::StaleAction { action: A11yAction::from_accesskit(request.action) });
        return;
    };
    if app.window(window).is_none_or(|w| w.tree().get(engine).is_err()) {     // 陈旧/跨窗口
        app.a11y_note(A11yDiagnostic::StaleAction { .. });
        return;
    }
    // ① 应用显式 handler 优先（除 Click —— 它必须落到 Action 表，见 §9.3）
    if !matches!(action, A11yAction::Click) && app.a11y_handler(action).is_some() { … }
    // ② 内置映射（表见下）
}
```

| `accesskit::Action`                                                                                                         | 引擎效果                                                                                                              | 备注                                                                                                       |
|-----------------------------------------------------------------------------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------|
| `Click`                                                                                                                     | `focus::action_target(tree, node)` → `App::dispatch_action(window, target, action, ActionSource::Programmatic)`       | **不合成 `PointerDown/Up`**：P10 §6.4 已裁决（合成指针会触发拖拽/双击/浮层外点关闭）。无声明 ⇒ 忽略 + 诊断 |
| `Focus`                                                                                                                     | `focusable(tree, node)` 成立 ⇒ `App::set_focus(window, Some(node))`；否则忽略                                         | 校验与 Tab 序同源（`focus.rs:21`）                                                                         |
| `Blur`                                                                                                                      | 焦点正好是该节点 ⇒ `App::set_focus(window, None)`                                                                     | 否则 no-op                                                                                                 |
| `SetValue`（`ActionData::Value`）                                                                                           | 应用 handler `A11yAction::SetValue`，`data = Value(&str)`                                                             | 引擎没有文本模型（§1.2）                                                                                   |
| `ReplaceSelectedText`                                                                                                       | 同上（`A11yAction::SetValue` + `ActionData::Value`）                                                                  | v1 与 `SetValue` 等价（无选区模型）                                                                        |
| `SetTextSelection`（`ActionData::SetTextSelection`）                                                                        | 应用 handler `A11yAction::SetTextSelection`，`data = Selection{anchor, focus}`（**字符索引**，转字节/字形是应用的事） | `text::inline_flow` 的选区在应用层（§4.7）                                                                 |
| `ScrollUp/Down/Left/Right`（`ActionData::ScrollUnit`）                                                                      | 目标（或其 `scroll_ancestor`）的 `ScrollHandle` → `App::scroll_by(handle, delta)`                                     | `Item` ⇒ `line_height`（`ScrollSlot.line_height`），`Page` ⇒ 视口高度                                      |
| `ScrollIntoView`                                                                                                            | `App::scroll_to` 把目标矩形滚入视口（最小移动）                                                                       | `ScrollHint` 参与计算（v1 只用 `TopLeft`/`BottomRight`/`*Edge` 的最近边）                                  |
| `SetScrollOffset`（`ActionData::SetScrollOffset(Point)`）                                                                   | `App::scroll_to(handle, point)`                                                                                       |                                                                                                            |
| `Expand` / `Collapse`                                                                                                       | 应用 handler `A11yAction::{Expand,Collapse}`                                                                          | v1 无引擎侧展开语义（浮层开合在应用）                                                                      |
| `ShowContextMenu`                                                                                                           | 应用 handler `A11yAction::ShowContextMenu`                                                                            |                                                                                                            |
| `CustomAction`（`ActionData::CustomAction(id)`）                                                                            | 应用 handler `A11yAction::Custom(id)`                                                                                 |                                                                                                            |
| 其余未列（`Decrement`/`Increment`/`HideTooltip`/`ShowTooltip`/`ScrollToPoint`/`SetSequentialFocusNavigationStartingPoint`） | 忽略 + `UnhandledAction`                                                                                              | 显式不做（§4.2）                                                                                           |

滚动路径的**语义约束**（照 P10）：`scroll_by`/`scroll_to` 只写 `ScrollSlot.offset` + `Window::request_damage(viewport)`

+ `wake_requested`（S-98），**不产生脏位、不跑 taffy**；a11y 的 bounds 随之变化由
  `A11yBridge::invalidate_subtree(container)` 补齐（§5.6）——所以 `action.rs` 在滚动成功后必须调它，
  否则 AT 读到的是旧几何。

### 9.2 陈旧 `NodeId` 的处理（忽略 + 日志）

三级校验，全部在 `dispatch` 里完成，**任何一级失败都只记诊断**：

| 级别 | 检查                                 | 失败原因                          |
|------|--------------------------------------|-----------------------------------|
| 1    | `A11yNodeId::to_engine()`            | 代际 0 的编码（不可能，防御）     |
| 2    | `window.tree().get(engine).is_err()` | 槽位已回收/从不存在 ⇒ `StaleNode` |
| 3    | 目标是否属于该窗口                   | `NodeId` 只在窗口内有意义（S-87） |

`A11yDiagnostic::StaleAction { action: Option<A11yAction> }`（`None` = 该 accesskit 动作未映射；**不带 NodeId**：陈旧 id 的
`Debug` 会误导，用 `u64` 明文更诚实）。
probe ① #14 已证明"同槽位下一代 id"在引擎侧查不到。

### 9.3 为什么 `Click` 必须走 `Action` 表

P10 A1 的裁决：`Action` 是 handler 表的键，声明在节点（`on_click`）、handler 在 `App`（`on_action`）。
a11y 的 `Click` 就是"激活这个声明"，与键盘 Enter/Space、指针点击是**同一个身份**（F-93 的理由）。
再挂一张 `A11yAction::Click` 的表会让同一个语义有两个注册入口。因此：`A11yAction::Click` **不出现在**
`on_a11y_action` 的合法键里（注册时报 `A11yDiagnostic::UnhandledAction`），只能用 `App::on_action`。

### 9.4 禁止在 a11y 回调里改树

`do_action` 在 UIA 线程（§8.4）⇒ 只能 `Dispatcher::push`。主线程的 `dispatch` 全程走：
`set_focus` / `dispatch_action` / `scroll_*` / 应用 handler —— 四者都已经在 P03/P10 里走通"写状态 → 标脏 →
`wake_requested`"的闭环（§3.6）。a11y 不引入第二条写路径。

---

## 10. 测试

> 全部落在 `cargo test -p ui_core` 与 `--features test-support`；无 GPU、无桌面、无辅助技术。
> 单测内联在被测文件末尾（M-04），跨文件共享的建树辅助只放在对应模块的 `#[cfg(test)]` 内。

### 10.1 `A11yTreeSnapshot` 断言（CPU，`test_support`）

捕获路径：`TestApp::drain` 在 `App::frame` **返回之后**调 `App::a11y_take` + `App::attach_a11y`（§7.2；与平台泵的
`update_a11y` 同序）。`TestApp` 新增：

```rust
pub fn enable_a11y_capture(&mut self);       // 打开 bridge.capture（首帧 full = true）
pub fn a11y(&self) -> Option<&A11yTreeSnapshot>;  // = frame_record().a11y.as_ref()
```

| #  | 用例                                                 | 断言                                                                                                                                                            |
|----|------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 1  | `a11y_roles_labels_values`                           | 根 `Window`；`text` 节点 `Label` + `value == "价格"`；button（`a11y(over)`）`Button` + `label == "提交"`；input `TextInput` + `value`；canvas `Image` + `label` |
| 2  | `a11y_increment_contains_only_changes_and_ancestors` | 改一个 `bind_a11y` 的 label ⇒ 第二次快照 `full == false`，`nodes` 集合 == {该节点} ∪ 其祖先；不含兄弟                                                           |
| 3  | `a11y_focus_is_always_present`                       | 每次快照 `focus` 都指向 `nodes` 里存在（或上一次交付已存在）的节点；`focus_node()` 恒 `Some`                                                                    |
| 4  | `a11y_structure_matches_tree`                        | 增删节点后，快照里父的 `children` 与 `tree.children(parent)` **逐项相等**（含卸载）                                                                             |
| 5  | `a11y_bounds_equal_probe_times_scale`                | `snap.node_for(n).bounds == probe.bounds(n).to_physical(scale)`（scale 1.0 与 1.25 两组）                                                                       |
| 6  | `a11y_bind_a11y_disabled_is_not_visual`              | `bind_a11y(                                                                                                                                                     |cx| A11yPatch { disabled: Some(cx.read(&disabled)), .. })` 翻转 ⇒ 快照 `disabled == true`、节点 `!LAYOUT && !PAINT`、`record.damage` 空、`FrameOutcome::Idle` |
| 7  | `a11y_canvas_requires_alt_text`                      | 无 `a11y(A11yOverride{label})` 的 canvas ⇒ 快照里 `hidden == true` + `a11y_diagnostics()` 含 `MissingAltText`；**节点仍在快照里**（不静默丢弃）                 |
| 8  | `a11y_hidden_is_excluded`                            | `visible: Some(false)` / `Display::None` / `A11yPatch.hidden` ⇒ `snap.node(x).hidden`；`visible_nodes()` 不含它及其子树                                         |
| 9  | `a11y_interactive_false_is_disabled_not_hidden`      | `StylePatch.interactive = Some(false)` ⇒ `disabled == true`、`hidden == false`                                                                                  |
| 10 | `a11y_first_delivery_is_full_then_incremental`       | 首快照 `full == true` 且 `nodes.len() == tree.order().len()`；无变化 ⇒ `delivered == false`（O(1) 早退）                                                        |
| 11 | `a11y_scale_change_resends_full`                     | `window.set_scale(1.5)` ⇒ 下一次快照 `full == true`、bounds 全部按 1.5 重算                                                                                     |
| 12 | `a11y_scroll_resends_subtree`                        | `scroll_to(container, y)` ⇒ 快照含容器的已挂载子节点、bounds 已平移                                                                                             |
| 13 | `a11y_virtual_list_exposes_only_mounted_rows`        | 100 行、视口 5 行 ⇒ 快照里行数 == `virtual_list_state().mounted`；滚到末尾后旧行消失                                                                            |
| 14 | `a11y_order_bit_is_consumed`                         | `mark_structure` 后快照产出 ⇒ 节点不再含 `ORDER`（V-65 关闭）                                                                                                   |

### 10.2 `build_update` 的直接单测（无平台、无 `TestApp`）

`a11y/adapter.rs` 的 `#[cfg(test)] mod tests` 直接对 `build_update` 断言 `TreeUpdate` 的**字段级**契约
（这些断言正是 probe ① 在仓库里的固化形态）：

| #  | 断言                                                                                                                                                                       |
|----|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 15 | 首次：`update.tree == Some(TreeInfo{ root, toolkit_name: Some("Kytrix"), toolkit_version: Some(env!(..)) })`、`tree_id == ROOT`                                            |
| 16 | 增量：`update.tree.is_none()`、`nodes` 只含变更节点 + 祖先（逐个 id 比对）                                                                                                 |
| 17 | 焦点：只移动焦点 ⇒ `nodes.is_empty()`、`focus == 新焦点`（probe ① #11 的仓库化）                                                                                           |
| 18 | 焦点回退：焦点指向未发出节点 ⇒ `focus == 最近已发祖先` + `MissingFocusTarget`                                                                                              |
| 19 | 结构新增：父的 `children` 含新子，且新子在 `nodes` 里（probe ① #9 的仓库化）                                                                                               |
| 20 | 卸载：父的 `children` 不含被删 id，且被删 id **不出现**在 `nodes` 里（probe ① #10）                                                                                        |
| 21 | `hidden` / `disabled` / `selected` / `value` / `label` 落到对应的 `accesskit::Node` setter（用 getter 回读：`node.is_hidden()` / `label()` / `value()` / `is_selected()`） |
| 22 | `bounds` 的 `Rect { x0.. }` 等于 `PhysicalRect` 的四个整数（`f64` 精确相等，无浮点误差）                                                                                   |

### 10.3 Windows adapter：只编译 / 构造

`platform/windows/a11y.rs` 的 `#[cfg(test)] mod tests`（`#[cfg(windows)]` 下自动生效）：

| #  | 用例                                              | 断言                                                                                                                                 |
|----|---------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------|
| 23 | `adapter_constructs_before_show`                  | 建隐藏窗口（`CreateWindowExW` 但不 `ShowWindow`）→ `WindowsA11y::new` 返回 `Ok`；`IsWindowVisible == false`                          |
| 24 | `update_if_active_is_lazy_without_assistive_tech` | 无 AT 时 `update_if_active(                                                                                                          || unreachable!())` 返回 `None`（闭包不执行）—— 在 CI 无 AT 环境下**确定性成立**（probe ② #2/#4） |
| 25 | `focus_state_without_assistive_tech`              | `focus(true)` / `focus(false)` 都返回 `None`，不 panic                                                                               |
| 26 | `wm_getobject_objid_filter`                       | 非 client/root 的 objid ⇒ `wm_getobject` 返回 `None`（交 `DefWindowProcW`）；**不**断言 `Some`（有 AT 的机器上会真的激活，非确定性） |

> 用例 24 的措辞：CI 若有 NVDA 在跑会变成"有 AT"，那时应改为 `is_none_or(|_| true)` 的宽松断言。写进
> 测试的 `//` 注释（与 P09 的 `dispatcher` 已知 flake 同类的环境依赖，V-73 的处理方式）。

集成测试 `tests/frame_record.rs`：加一条 `a11y` 断言（`snap.focus_node()` 存在、`full` 首帧为真）。

### 10.4 `--features test-support` 与发布物

`A11yTreeSnapshot` 的**类型**无条件编译（`a11y.rs`，它是公开 API）；快照的**捕获**只在 `test-support` 下接线
（`App::frame` 的 `#[cfg(feature = "test-support")]` 分支）。`capture` 字段本身无条件存在（一个 `bool`），
生产构建里恒 `false`——避免再引一层 cfg。

### 10.5 测试不得依赖的假设

- 不假设 `A11Y` 位会在帧末被清（它不会）；断言用"快照产出后不再含 `A11Y`"。
- 不假设 `update_if_active` 在 CI 上返回 `Some`（有没有辅助技术取决于机器）。
- 不用 `accesskit_consumer` 做断言（它不是引擎的依赖；跨树规则由 probe ① 在仓库外验证，本文 §2.3 记录结论）。

---

## 11. 公开面 / 死代码 / 规范

### 11.1 `Node` 尺寸预算（1144 B → **仍 1144 B**）

| 方案                                                                      | `size_of::<Node>()` | 评价                                                                                                                           |
|---------------------------------------------------------------------------|---------------------|--------------------------------------------------------------------------------------------------------------------------------|
| `Node.a11y: Option<Box<NodeA11y>>`（+8）                                  | **1152**            | 正好顶到上限（F-102 的 1152），P12 无余量                                                                                      |
| **侧表 `NodeTree.a11y_slots: Vec<Option<Box<NodeA11y>>>`（采纳，F-114）** | **1144（不变）**    | 与 `layout_cache`（S-42）、`clip_keys`（S-66）、`text_slots`、`debug_ids` **同型**，不引入新机制；只有声明了 a11y 的节点才分配 |

侧表的槽位复用纪律与 `clip_keys` 完全一致（`insert_child` 写/清、`remove` 逐槽清）——两条既有用例
（`slot_reuse_drops_cache` / `clip_key_roundtrip_and_clear`）的形态可直接照搬。 **`node_size_after_p06` 的 1144 断言不变**
（P11 不加字段，F-114）。

### 11.2 无 `#[allow]` / 无 `#[expect]` / `unsafe` 限额

| 项                                  | 要求                                                                                                                                                                 |
|-------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `#[allow(...)]` / `#[expect(...)]`  | **0 处**（AGENTS §4；P10 的验收口径：`grep -c '#\[allow(' == 0`）                                                                                                    |
| `#[path]`                           | 0 处（M-01）                                                                                                                                                         |
| `pub use`                           | 0 处；内部别名用 `pub(crate) use`（M-07），P11 实际不需要                                                                                                            |
| `unsafe`                            | 只在 `platform/windows/**`（§8.6 三处，每处 `// SAFETY:`）；`a11y/**` 全安全 Rust                                                                                    |
| 死代码                              | `pub(crate)` 项必须有**非 test 消费者**（§3.1 表逐条给出）；`pub` 项进白名单（§3.2）                                                                                 |
| 每类型一个 inherent `impl`          | `A11yBridge` / `A11yTree` / `A11yNode` / `A11yTreeSnapshot` 各一个块（`multiple_inherent_impl = "forbid"`）；`Window` / `App` / `NodeTree` 的方法写进**已有**的 impl |
| `definition_in_module_root`         | `a11y.rs` 是**自命名模块根**（`self_named_module_files = "allow"`，与 `interaction.rs` 同款），允许定义契约类型；`a11y/*.rs` 才放实现                                |
| `missing_docs`                      | 全部公开项（含 `A11yNode` 的每个字段）必须有 `///`                                                                                                                   |
| `field_scoped_visibility_modifiers` | 侧表载荷 `NodeA11y` 用「`pub(crate)` 结构 + `pub` 字段」（`NodeInteraction` 同款）                                                                                   |

### 11.3 验收命令（实现期）

```powershell
cargo test -p ui_core
cargo test -p ui_core --features test-support
cargo +nightly clippy -p ui_core --all-targets --all-features -- -D warnings
cargo +nightly fmt --all -- --check
cargo doc -p ui_core --no-deps
cargo build -p ui_core --release
```

`cargo clippy --workspace --all-features` 仍会在 `llama_sys` 失败（P10 的已知环境限制），门禁按 `-p` 收窄。

---

## 12. P12 交接（macOS / Unix adapter 的接口）

本阶段只把**平台无关**的部分定死，平台文件留空位。P12 落地时必须填的三张表：

| 项   | macOS（`platform/macos/a11y.rs`）                                                                                              | Linux（`platform/linux/a11y.rs`）                                                                                            |
|------|--------------------------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------|
| 构造 | `unsafe { accesskit_macos::Adapter::new(ns_view, is_focused, action_handler) }`（`*mut c_void`）                               | `accesskit_unix::Adapter::new(activation_handler, action_handler, deactivation_handler)`（**要 activation + deactivation**） |
| 焦点 | `update_view_focus_state(bool) -> Option<QueuedEvents>`                                                                        | `set_root_window_bounds(outer, inner)`（没有窗口焦点态）                                                                     |
| 更新 | `update_if_active(f) -> Option<QueuedEvents>` + `raise()`                                                                      | `update_if_active(f)` —— **无返回值**，事件由 AT-SPI 广播，不需要 `raise`                                                    |
| 命中 | `hit_test(point: NSPoint, &mut activation_handler)` / `view_children` / `focus`（**没有** `register_hit_test`，附录 A.3 N15①） | 无（AT-SPI 走对象路径）                                                                                                      |
| 关闭 | drop `Adapter`                                                                                                                 | **必须** `DeactivationHandler::deactivate_accessibility()`（唯一真正需要它的平台）                                           |

共享层已经为此留好的接口（P11 定死，P12 直接用）：

- `A11yBridge::build_update(app, window) -> accesskit::TreeUpdate` —— 与平台无关，三个平台共用。
- `WindowsActivation`（`request_initial_tree` 恒 `None`）的形态对 macOS 同样适用（macOS 的 activation handler
  在 `hit_test`/`view_children` 里被调用，返回 `None` 会走 placeholder 语义）；**unix 的 activation handler
  不同**（它在 `Adapter::new` 里传入，且 `request_initial_tree` 必须返回全树——AT-SPI 没有 placeholder 概念）。
  ⇒ `a11y/adapter.rs` 需要一个 `pub(crate) fn full_update(app, window) -> TreeUpdate`（`full_pending` 强制为真），
  P12 的 unix 路径用它。**P11 就实现它**（Windows 的首次路径也复用它，不写两份）。
- `A11yActionHandler` 的"只入队"形态对三平台通用（`Dispatcher::push` 是平台无关的）。
- `a11y/action.rs::dispatch` 平台无关（不碰原生句柄）。

---

## 13. 风险与待复核

| 编号 | 项                                                                           | 影响 | 缓解 / 归属                                                                                                  |
|------|------------------------------------------------------------------------------|------|--------------------------------------------------------------------------------------------------------------|
| V-75 | UIA 真实联动（角色/名称/动作/焦点事件）未验证                                | 中   | L5 实机矩阵（NVDA / Narrator）；本文只保证 `TreeUpdate` 的**数据**正确（probe ① 已用真正的消费端校验）       |
| V-76 | macOS / Linux adapter 完全未验证                                             | 中   | P12；接口见 §12                                                                                              |
| V-77 | `QueuedEvents::raise()` 的路径未在真实 AT 下执行过（probe ② 刻意 drop）      | 中   | L5；`raise` 的线程约束已按 `adapter.rs:633-642` 落实                                                         |
| V-78 | 大树（> 5k 节点）首次全树的成本未基准（一次 `Vec<(NodeId, Node)>` 全量构造） | 低   | P11 只在首次激活时付；若进 L4 基准且超标 ⇒ 分片交付（`TreeUpdate` 可多次调用），不改 `A11yTreeSnapshot` 契约 |
| V-79 | `A11yPatch.selected/expanded` 的引擎侧写点很少（多数控件由应用给）           | 低   | 字段有非 test 消费者（映射），符合 AGENTS §2.1；若 P13 的控件库不需要则删除                                  |
| V-80 | `pos_in_set` / `set_size_of_set` 未填（`VirtualListState` 已有数据）         | 低   | §5.6 已记；需要时补 `Node::set_pos_in_set`（`lib.rs` 有该 setter）                                           |

**需上级裁决（5 条）**：

1. `A11yTreeSnapshot` 放 `FrameRecord` 而非 `RenderedFrame`（§0 #1 / F-113）。
2. `mark_layout` 追加 `A11Y`（§6.2 / S-109）—— 改 P03 的脏位语义。
3. 文本 → `Role::Label` + `set_value`（§5.2 / F-106 / F-107）—— 主设计 §7.4 表的事实错误。
4. 被 clip 完全裁掉的节点**不**标 `hidden`（F-112；v1 只按 `visible`/`Display`/`A11yPatch.hidden`/缺替代文本）。
5. 新增 `App::on_a11y_action` 表（`A11yAction` 为键、payload 在 `A11yActionCtx`）—— 承载 `SetValue` /
   `SetTextSelection` / `Expand` / `Collapse` / `ShowContextMenu` / `Custom`。

---

## 14. 实施记录（2026-11-13，P11 落地）

| 项         | 实测                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
|------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 新增       | `src/a11y.rs` + `a11y/{tree,mapping,adapter,action}.rs`；`platform/windows/a11y.rs`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| 修改       | `lib.rs`（+`pub mod a11y;`）、`app.rs`（`refresh_styles` 的 `bind_a11y` 同批求值 / `a11y_take` / `a11y_pending` / handler 表 / 诊断 / `attach_a11y`）、`window.rs`（`a11y` 槽 + `set_scale` 换代 + `attach_a11y`）、`node/tree.rs`（`a11y_slots` 侧表 + `a11y_dirty` 计数 + `clear_a11y` + `mark_layout`/`mark_subtree_paint` 追加 `A11Y`）、`node/builder.rs`（`a11y`/`bind_a11y`）、`error.rs`（`A11yInit`）、`platform/windows/{window,event_loop,}.rs`（首显前构造 / `WM_GETOBJECT` / 焦点消息 / 泵的 ④′）、`test_support/{app,record}.rs`（捕获 + `FrameRecord.a11y`） |
| 测试       | `cargo test -p ui_core` **589**（P10 基线 557 → **+32**）；`--features test-support` **621**（再 +32 条端到端：`p11_tests` / `p11_fix_tests`）+ 集成 **6**；demo 1                                                                                                                                                                                                                                                                                                                                                                                                          |
| 门禁       | clippy `-p ui_core -p ui_core_demo --all-targets --all-features -D warnings` 通过；`fmt --check` 通过；`doc --no-deps` 无 warning；`--release` 通过                                                                                                                                                                                                                                                                                                                                                                                                                         |
| 死代码     | `#[allow(` 0 / `#[expect(` 0 / `#[path` 0 / `pub use` 0；`a11y*` 的 `unsafe` **0** 处                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| 尺寸       | `size_of::<Node>()` = **1144 B**（不变；侧表方案生效，F-114）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| 文件边界   | `platform/` 下只有 `windows/a11y.rs`（macOS / Linux 留 P12）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| P03 断言   | `mark_layout` 追加 `A11Y` 后，两条 flags 用例**无需改**（它们用 `contains`）——全量用例原样通过（S-116）                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| 实施期偏差 | F-115（虚拟列表角色顺序）… F-118、S-114…S-118、V-81：见 `design/DEVIATIONS.md` §16.4 / §16.5                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |

### 14.1 审查修复轮（PASS-WITH-FIXES → 已修）

| # | 问题                            | 修复                                                                                                                                                |
|---|---------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------|
| 1 | 布局后子树 bounds 不重发        | `NodeTree::mark_moved_a11y()` + `Window::compute_layout` 调用；`set_size`/`set_scale` 改走 `invalidate_layout`                                      |
| 2 | `invalidate_layout` 未标 `A11Y` | 统一追加（覆盖 `set_style`/`invalidate_measure`/`set_layout_override`/`set_virtual_row`/`set_absolute_inset`）                                      |
| 3 | 普通滚动不失效 a11y 子树        | 移到 `App::scroll_to` 成功路径（`slot.offset` 的唯一写点，动画滚动也走它）                                                                          |
| 4 | `focus_node()` panic            | 快照新增 `focus_node: A11yNode`（独立于 `nodes`），不再查表                                                                                         |
| 5 | `WM_GETOBJECT` 重入 UB          | `wm_getobject` 返回未转换值、`wndproc` 出借用块后才 `.into()`；`raise()` 同纪律；`begin/end_getobject` 覆盖整个转换窗口                             |
| 6 | `bind_a11y` 写入仍重绘          | 绑定类型分流（`has_visual_bind`/`has_a11y_bind` + `bind_work`）、`needs_frame` 感知、`paint_frame` 无像素早退；用例断言 `Idle`/无 damage/无 `PAINT` |

`A11yTreeSnapshot` 的字段在本轮定为：`full` / `nodes` / `focus` / `focus_node` / `diagnostics`。

---

## 附录 A：probe 骨架（`%TEMP%\kg-p11-a11y`，仓库外）

`Cargo.toml`：

```toml
[package]
name = "kg-p11-a11y"
version = "0.1.0"
edition = "2024"

[dependencies]
accesskit = "=0.25.2"
accesskit_consumer = "=0.39.2"

[target.'cfg(windows)'.dependencies]
accesskit_windows = "=0.35.2"
windows = { version = "0.62.2", features = [
    "Win32_Foundation", "Win32_Graphics_Gdi", "Win32_System_LibraryLoader",
    "Win32_UI_WindowsAndMessaging",
] }
```

`src/bin/cpu_tree.rs` 的核心（完整版 400 行，含 16 条断言）：

```rust
fn ak_id(index: u32, generation: u32) -> NodeId {          // 引擎 NodeId::to_bits 的编码
    NodeId(((generation as u64) << 32) | index as u64)
}
fn local(id: FullNodeId) -> NodeId {                      // 消费端只给 u128: (local << 64) | tree_index
    NodeId((u128::from(id) >> 64) as u64)
}

struct Eng {
    id: NodeId,
    role: Role,
    label: Option<String>,
    value: Option<String>,
    hidden: bool,
    actions: Vec<Action>,
    children: Vec<NodeId>,
    bounds: (f64, f64, f64, f64)
}

impl Eng {
    fn to_accesskit(&self, scale: f64) -> Node {
        let mut node = Node::new(self.role);
        if !self.children.is_empty() { node.set_children(self.children.clone()); }
        if self.hidden { node.set_hidden(); }
        if let Some(l) = &self.label { node.set_label(l.clone()); }
        if let Some(v) = &self.value { node.set_value(v.clone()); }
        for a in &self.actions { node.add_action(*a); }
        let (x, y, w, h) = self.bounds;
        node.set_bounds(Rect::new((x * scale).round(), (y * scale).round(),
                                  ((x + w) * scale).round(), ((y + h) * scale).round()));
        node
    }
}

// 断言序列（§2.5 表逐行）
let mut tree = Tree::new(full_update, true);
assert_eq!(local(tree.state().root_id()), root);
assert_eq!(filtered_child_ids(&tree, root), vec![text, button, input, canvas, scroll]);
// 只改 label ⇒ nodes.len() == 1
// 新增子 ⇒ 必须同批发父；只发子 ⇒ catch_unwind 证明 panic
// 删除 ⇒ 只发父；removed 含被删 id
// 焦点 ⇒ nodes: [] 合法；ghost 焦点 ⇒ catch_unwind 证明 panic
// 陈旧 id ⇒ node_by_tree_local_id(..).is_none()
```

`src/bin/win_adapter.rs` 的核心（完整版 190 行）：

```rust
unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(h, m, w, l) }
}
let hwnd = CreateWindowExW(..WS_OVERLAPPEDWINDOW ..).unwrap();   // 不 ShowWindow
assert!(!unsafe { IsWindowVisible(hwnd) }.as_bool());
let mut adapter = Adapter::new(hwnd, false, Handler);
assert!(adapter.update_if_active(|| {
    calls += 1;
    full_update()
}).is_none());
assert_eq!(calls, 0);                                             // Inactive ⇒ 闭包不执行
unsafe { let _ = ShowWindow(hwnd, SW_SHOWNORMAL); }
assert!(adapter.update_if_active(|| {
    calls += 1;
    full_update()
}).is_none());
assert_eq!(calls, 0);                                             // 显示后仍零开销
let mut activation = Activation;                                  // request_initial_tree -> None
assert!(adapter.handle_wm_getobject(WPARAM(0), LPARAM(-25), &mut activation).is_some());
assert!(adapter.update_if_active(|| {
    calls += 1;
    full_update()
}).is_some());
assert_eq!(calls, 1);                                             // 占位激活后闭包才执行
assert!(adapter.handle_wm_getobject(WPARAM(0), LPARAM(-1), &mut activation).is_none());
```

## 附录 B：复现

```powershell
$env:CARGO_TARGET_DIR = "$env:TEMP\kg-p11-a11y-target"
cd $env:TEMP\kg-p11-a11y
cargo run --offline --bin cpu_tree
cargo run --offline --bin win_adapter
cargo run --offline --bin wm_getobject_pattern
```

## 附录 C：probe 输出

```text
cpu_tree probe OK
  accesskit 0.25.2 / accesskit_consumer 0.39.2
  nodes = 11, focus moves = 3, 全部断言通过

win_adapter probe OK
  Adapter::new 在 IsWindowVisible == false 时构造成功（首显前构造可行）
  Inactive（显示前/显示后，无 AT）：闭包执行 0 次
  WM_GETOBJECT(OBJID_CLIENT) → Some，随后 update_if_active 闭包执行 1 次
  WM_GETOBJECT(其它 objid) → None（DefWindowProcW 兜底）

wm_getobject_pattern: 借用作用域 + 作用域外 .into() 编译通过
```

> `cpu_tree` 的两条 panic 输出（`… neither in the current tree …`、`Focused ID … is not in the node list`）
> 是 `catch_unwind` 刻意捕获的反例证据，不是失败。
