# Phase 10 · 交互机制 + Action 分发 — 详细设计（v1）

> 依据：`custom-gui-engine-design.md` §3.6（事件 → Action → 状态 → 失效闭环）/ §3.7（高频状态与动画）/ §4.4（`hit_test`）/
> §4.9（`Style` / `StylePatch`）/ §7.7 / §7.8（a11y 的 Action 与焦点映射，本阶段**只留接口**）/ §8.4（通用交互机制）/ 附录 A；
> `design/phase-03-node-state.md`（`ReadCtx` / `Signal` / `Transient` / `DirtyFlags` / §9.5）/ `design/phase-04-layout.md`
> （§10 `LayoutOverride` / §12 命中测试）/ `design/phase-06-paint-runtime.md`（§5 lane / §9 帧调度 / §10.4 `inject`）/
> `design/phase-09-win32-demo.md`（§4 事件映射 / §5.2 消息泵 / §7 demo）；
> 已落地代码：`event.rs` / `platform/**` / `app.rs` / `window.rs` / `node/**` / `layout/hit_test.rs` / `paint/{pass,style}.rs` /
> `test_support/**`；DEVIATIONS A-01 / M-02 / M-03 / M-06 / F-45 / F-58 / F-59 / F-66 / F-86 / S-79 / S-82 / V-36 / V-38。
>
> 本文 §N 指本文件。本轮**只做设计 + 本机 probe**：不写仓库代码、不改 `Cargo.toml` / `Cargo.lock`、不 commit。
> probe 全部产物在 `%TEMP%\kg-p10-interaction`（仓库外），复现见 §2.2。

---

## 0. 结论摘要（先给定死项）

| # | 定死项 | 一句话依据 |
|---|--------|-----------|
| 1 | **`Action` 是引擎枚举**（`FocusNext` / `FocusPrev` / `Dismiss` / `Custom(SmolStr)`），节点只在**声明侧**持有它（`on_click` / `on_drag`）；**handler 集中在 `App` 的注册表**（`App::on_action`），闭包不上节点 | 节点上放闭包 = 每个交互节点一次分配 + 无法被 `KeyBinding`/a11y 复用同一身份；集中注册让「点击」与「快捷键」落到**同一个** handler |
| 2 | **分发顺序固定**：命中/焦点节点 → 祖先冒泡（找 `on_click`/`on_drag` 声明）→ `Action` → 全局 handler 表。冒泡在 `interactive == Some(false)` 处**停止** | 与浏览器/GPUI 的直觉一致且可单测；`interactive:false` 同时把节点从 `hit_test` 的目标集里去掉（§4.6） |
| 3 | **焦点单一权威源 = `Window` 的 `FocusScope`**；Tab 序 = `tree.order()`（DFS 先序）**过滤**，**不做 `tab_index`**；焦点环由 paint 画在**焦点节点自己的 lane 内**（`Style.focus_ring`），因此受祖先 clip 约束且不引入第二套绘制通道 | 第二套排序会造成 `ORDER` 与 `tab_index` 双权威；独立 overlay lane 会绕过 clip（浮层外可见） |
| 4 | **滚动偏移是 `Transient`，只在 paint 与 `hit_test` 两处应用**（`paint_node` 的累计偏移；`layout.bounds` 恒为内容坐标、`absolutize` 不读 offset）；滚动**不标脏位、不跑 taffy、不跑 `refresh_styles`**，只产生**视口矩形**的 damage | 把偏移写进 `layout.bounds` 会每帧重排 + 重建整棵子树（违背 §3.7 / D16 / R-1）；paint 期应用后，脏位表里根本没有「滚动」这一行 |
| 5 | **平台侧只新增三类对外行为**：光标（类光标改 `IDC_ARROW` + 泵里按需 `SetCursor`；probe：null 类光标 + `HTCLIENT` 会得到**空光标**）、指针捕获（`SetCapture`/`ReleaseCapture`，拖拽）、动画定时器（`SetTimer(1 ms)` + `WM_TIMER`，probe 实测 16.0 ms）。**不新增 `WM_SETCURSOR` 分支**（`DefWindowProcW` 已按类光标处理客户区与非客户区） | probe §2：`HTLEFT`/`HTBOTTOMRIGHT` 等非客户区由 `DefWindowProcW` 自行设尺寸光标（返回 1），只有 `HTCLIENT` 在类光标为 null 时是空光标 |
| 6 | **虚拟列表 = 滚动容器 + 均匀行高的绝对定位行**；引擎只挂载可见窗口（+ overscan），离屏行 `remove`；`content` 尺寸由 `count × extent` 显式给出（引擎不猜内容盒） | taffy 没有「可滚动内容盒」概念；自动内容尺寸只能靠遍历子树推，且行复用后再推会漂移 |
| 7 | **浮层 = root 的最后若干子节点**：层级 = 绘制序 = `order` 末尾，命中优先级因此**免费**得到（`hit_test` 逆序）；点击外部关闭只处理**最上层**同时满足 `dismiss == OutsideClick` 的那一个，并**消费**该次按下 | 不引入 `z-index` 第二套排序；`append_child(root, …)` 天然让浮层在 `order` 末尾（`hit_test.rs:444` 已按此契约写用例） |
| 8 | **拖拽 = 会话 + `Transient` 增量 + 应用写 `LayoutOverride`**；引擎不在拖拽里改布局（阈值、夹紧、业务几何都属应用） | `LayoutOverride` 是 P04 的 root 逃生口（§10.2），引擎替应用算分栏矩形等于把 widget 几何塞进引擎（违反 D15 分层） |
| 9 | **动画 = 显式注册的节点集合 + `Transient` 写入**；`AnimationClock` 在 `App` 里（它要同时拿 `&mut StateStore` 与 `&mut NodeTree`），每帧 tick 只对注册节点标 `PAINT`；结束即删槽 ⇒ `has_animations() == false` ⇒ 平台 `KillTimer` ⇒ 回 `Idle` | 时间信号若进依赖图会每帧全量失效（§3.7）；用 `Signal` 表达进度会让「每帧」变成「每帧一次失效广播」 |
| 10 | **`InputEvent` 的指针/滚轮变体加 `modifiers`**（平台侧已有真值 `TranslateState::modifiers`）；`InputOutcome` 从 `test_support::input` 迁到公开 `event.rs`，`test_support/input.rs` **删除** | `Shift+滚轮` 与 `Ctrl+点击` 必须在引擎侧可见；P06 的 `InputOutcome` 只有 `hit` 一个字段，P10 要扩到 `{hit, action, target, click_count}` |
| 11 | **`Node` 只加一个字段**：`interaction: Option<Box<NodeInteraction>>`（+8 B → **1136 B**，P06 预算 1128 / 上限 1152） | 滚动槽、虚拟列表槽、浮层槽、`on_click`/`on_drag` 全塞进同一个 `Box`，交互节点付一次分配 |
| 12 | **测试**：实际落地 **66** 项单测（`491 → 557`）+ **7** 项 `--features test-support` 端到端（`p10_tests`，`--features test-support` 共 574）+ 1 项 demo 用例；`TestApp` 增加 `advance(Duration)` 驱动动画/双击的确定性时钟 | 双击与动画都要时间源；生产用 `App::clock`（平台每轮 `sync_clock`），测试用 `advance`，**不引入 clock trait**（§11.6） |
| 13 | **a11y 只留接口**：`FocusScope::focused()`、`Overlay` 的 `order` 顺序、`ScrollState`、`VirtualListState` + `virtual_row_index`、`A11Y` 脏位的写点。P11 不落地任何 adapter | 主设计 §7.7/§7.8 的映射表要能对着具体 API 写，否则 P11 会重造第二份焦点/顺序真值 |

### 0.1 五条裁决（其余条目都在这五条之下）

| # | 裁决 | 被否掉的替代 | 依据 |
|---|------|--------------|------|
| **A1** | **Action 模型**：引擎枚举 + `Custom(SmolStr)`；声明在节点（`on_click`/`on_drag`）、handler 在 `App`；`ActionCtx` 携带 `{window, node, hit, modifiers, source}`。应用用自建的 `NodeId → 业务对象` 映射定位数据 | ①GPUI 形态 `on_click(cx.listener(…))`；②`Action` 只做 `Custom`；③`Action::Custom(SmolStr)` 带 payload 结构 | ①闭包在节点上 ⇒ 每节点一 alloc + 快捷键无法复用同一身份 + a11y 的 `ActionRequest` 无处映射；②引擎自己的 Tab/Esc 默认行为需要**可被覆盖**的身份；③payload 会让 `Action` 不可 `Hash`（handler 表键），且长度不固定 |
| **A2**（**上级裁决 2026-11-13：改为 paint 期偏移**） | **偏移是 `Transient`**（`Node.interaction.scroll.offset`，不登记依赖、不标 `LAYOUT`/`PAINT`）；`layout.bounds` 恒为内容坐标，`paint_node` 与 `hit_test` 各自按**累计偏移**换算；滚动只产生视口 damage，`FrameIndex` 照常推进 | ①吸收进 `layout.bounds`（**被否**：每帧重排 + 重建子树，违背 §3.7/D16）；②`App` 的 `Transient` 再双写进 tree（两个真值）；③per-lane 平移 + 编码器逐图元平移（`Scene`/`GpuPrimitive` 契约级改动，超 P10 边界，见 F-99） |
| **A3** | **焦点**：`FocusScope` 在 `Window`（每窗口一份）；Tab 序 = `order` 过滤；焦点环 = `Style.focus_ring` + paint 在节点 lane 内 | ①`tab_index` 第二排序；②焦点环用独立 overlay lane / 全屏后处理；③焦点存 `App`（按 `WindowId` 分组） | ①会让「结构顺序」与「Tab 顺序」两个真值同时存在，且 `ORDER` 脏位失去意义；②独立 lane 在窗口末尾 ⇒ 浮层外的环形也会画出来（clip 失效）；③a11y 与 paint 都只有 `&Window`，多一次查表且失去「窗口即作用域」 |
| **A4** | **虚拟列表**：容器 = 滚动容器（要求 `Clip::Bounds` + `position: relative`），行 = 绝对定位子节点（`left:0 / right:0 / top:i*extent / height:extent`），`content` 尺寸显式写 | ①`LayoutOverride` 行；②引擎自动测内容尺寸；③每次 `set_items` 全量重建行 | ①`LayoutOverride` 节点是布局叶子（P04 §10.3 表：**不能挂子节点**），而每行都要有文本/图标子节点；②taffy 无可滚动内容盒，遍历子树推出来的尺寸在回收后会漂；③全量重建会丢掉 `bind_style` 的依赖与 lane 复用 |
| **A5** | **平台三处**：类光标 `IDC_ARROW`、`SetCapture`/`ReleaseCapture`、`SetTimer(1)`+`WM_TIMER`；**不加 `WM_SETCURSOR` 分支**，光标由泵在「解析结果变化」时 `SetCursor` | ①保留 null 类光标 + 自己处理 `WM_SETCURSOR`；②每次 `WM_MOUSEMOVE` 都 `SetCursor`；③动画用 `SetTimer(16)` | probe：①null 类光标下 `HTCLIENT` 的 `DefWindowProcW` 返回 0 且光标变 **null**（不可见），引擎必须自己兜底；③`SetTimer(16)` 实测 26.7 ms（比 1 ms 的 16.0 ms 更慢且更抖） |

### 0.2 编号对齐（必读，影响 6 处既有注释）

本阶段是 **P10**。`phase-01` / `phase-03` / `phase-04` / `phase-09` 与部分代码注释里把交互机制写成了 **P13**、把光标写成 **P12**
（旧的阶段编号）。本阶段的落地改这些注释（纯注释，不改语义）：

| 文件:行 | 现文 | 改为 |
|---|---|---|
| `node/tree.rs:319`、`:949` | `P13 的分栏拖拽` | `P10 的分栏拖拽` |
| `layout/hit_test.rs:444`、`node/tree.rs:1180` | `P13 的 Overlay` | `P10 的 Overlay` |
| `paint/style.rs:101`、`:107` | `P12 设置/映射到平台光标` | `P10 设置平台光标` |
| `event.rs:95` | `P10 的 KeyBinding` | 不变（正确） |

**不改**：`app.rs:122`（`P11/P17` 指 macOS/Linux 端口）、`render.rs` / `platform.rs` 的端口编号、以及既有注释里的
主题/资源/a11y 树阶段号——它们属另一条编号线，改动会与既有 DEVIATIONS 冲突。**只在本阶段真正落地的机制上对齐编号。**

---

## 1. 范围、文件与非目标

### 1.1 本阶段交付（P10 实现期落地的文件）

```text
crates/kytrix_gui/src/
├── interaction.rs                     # 新：公共模块根（契约类型 + `pub mod` 子模块）
│                                      #   FocusScope / ScrollHandle / ScrollState / ScrollConfig / ScrollAxis
│                                      #   VirtualList / VirtualListConfig / VirtualListState
│                                      #   Overlay / OverlayOptions / OverlayPlacement / AnchorSide / OverlayDismiss
│                                      #   DragState / DragInfo / DragPhase / ActionSource / ActionCtx
│                                      #   AnimationId / AnimationSpec / AnimatedValue / Easing
│                                      #   InputThresholds / NodeInteraction(pub(crate))
├── interaction/
│   ├── focus.rs                       # 新：Tab 序、focusable、焦点移动、A11Y/PAINT 标记
│   ├── scroll.rs                      # 新：ScrollSlot、夹紧、平移子树、滚轮目标、平滑滚动
│   ├── virtual_list.rs                # 新：可见窗口、挂载/回收、行绝对定位
│   ├── overlay.rs                     # 新：打开/关闭/定位/外部点击
│   ├── drag.rs                        # 新：拖拽会话状态机
│   └── animation.rs                   # 新：AnimationClock 槽位、推进、缓动
├── event.rs                           # 改：+Action / +KeyBinding / +InputOutcome / InputEvent 加 modifiers
├── app.rs                             # 改：+dispatch_input / +dispatch_action / +交互写入口 / +动画 tick / +clock
├── window.rs                          # 改：+focus / +pointer 字段与只读面
├── node/tree.rs                       # 改：+Node.interaction、place_subtree 的 content shift / damage_clip
├── node/builder.rs                    # 改：+on_click / +on_drag
├── paint/style.rs                     # 改：+Style.focus_ring / +FocusRing
├── paint/pass.rs                      # 改：焦点环图元
├── platform/event.rs                  # 改：+PointerLeave、指针事件带修饰键
├── platform/windows/window.rs         # 改：类光标 IDC_ARROW（probe 依据）
├── platform/windows/event_loop.rs     # 改：WM_MOUSELEAVE / WM_TIMER / 捕获 / 光标 / 定时器
├── test_support/app.rs                # 改：inject 全量管线、advance(Duration)
└── test_support/input.rs              # **删**（InputOutcome 迁 event.rs；S-79 的同型处置）
```

**修改的既有行为（逐条，都是 additive 或有明确理由的最小改动）**

| 位置 | 改动 | 理由 |
|---|---|---|
| `event.rs` `InputEvent::{PointerMove,PointerDown,PointerUp,Scroll}` | 加 `modifiers: Modifiers` | `Ctrl+点击` / `Shift+滚轮` 需要；平台侧 `TranslateState::modifiers()` 已有真值 |
| `event.rs` | 加 `Action` / `KeyBinding` / `ActionHandler` / `InputOutcome` / `ActionSource` | P01 §4.5 的路径表点名 `event::{Action, KeyBinding}` |
| `node/tree.rs` `place_subtree` | 加 `damage_clip: Option<Rect>`，子节点参照系 = `abs.origin + content_shift(node)` | 滚动吸收进 bounds 的唯一实现点；damage 裁剪只对滚动路径生效（`None` = 现状） |
| `layout/hit_test.rs` `pick` | `interactive == Some(false)` 的节点**不作为自身目标**返回（子节点照旧参与） | §4.9 已把 `interactive` 定义为「命中/焦点/Action 开关」；否则禁用控件仍吃点击 |
| `paint/pass.rs` | 焦点节点额外发一个 ring `Quad` | 焦点环是 §8.4 明文分配给 `FocusScope` 的职责 |
| `window.rs` `Window` | 加 `focus: FocusScope` / `pointer: PointerState` | 每窗口状态；paint/a11y/平台都只有 `&Window` |
| `platform/windows/window.rs:289` | `hCursor: HCURSOR::default()` → `LoadCursorW(None, IDC_ARROW)` | probe §2：null 类光标下 `HTCLIENT` 得到空光标（不可见） |
| `platform/windows/event_loop.rs` | `WM_MOUSELEAVE` → `PlatformEvent::PointerLeave`；`WM_TIMER(id=1)` → `dispatcher.wake()`；每轮 `SetCapture`/`ReleaseCapture`、`SetCursor`、`SetTimer`/`KillTimer`、`app.sync_clock()` | §5.7 / §11.4 |
| `test_support/input.rs` | 删除 | 只剩 `InputOutcome` 一个类型，迁到公开 `event.rs` |

### 1.2 明确的非目标

| 不做 | 归属 |
|------|------|
| `accesskit_*` adapter、`TreeUpdate` 生成、`ActionRequest` 的接收入口 | P11（本阶段只留 §6.5 / §8.5 的读接口） |
| 文本编辑（插入符/选区/`SetValue`）、IME preedit 的交互、`TextInput` 角色 | 文本/IME Phase |
| 自绘标题栏 / `WindowControlArea` / 双击最大化 / 系统托盘 / 多窗口 | 窗口 Phase（P09 §1.2 的「P12」） |
| 主题切换的批量失效、`ColorResolver` 的调色板 | 主题 Phase |
| 资源/图标/图片解码 | 资源 Phase |
| 惯性曲线库（spring/物理模型）、`animate` 的插值空间选择 | 主题/动画后续 Phase（本阶段只有 `Linear` / `EaseOutCubic`） |
| 滚动条**控件**（thumb 绘制、拖拽 thumb、autohide） | 应用层 `base_ui`（引擎只提供 `ScrollState`） |
| 悬停（hover）的**状态与动画**（hover 高亮、tooltip 延迟） | 后续 Phase（v1 的 hover 只驱动光标；要能驱动业务就得先把 hover `Signal` 化，那属新需求） |
| 空间索引加速 `hit_test`（V-38） | 后续 Phase（基准证明需要时 additive 加） |

### 1.3 不变量落地

| 不变量 | 本阶段的落地点 |
|---|---|
| I3（空闲不提交帧） | `App::frame` 在 tick 之后才判 `needs_frame`；动画结束删槽 ⇒ 平台 `KillTimer`（§11.5） |
| I6（状态只能经 `Signal`） | handler 只能拿 `ActionCtx`（内部 `&mut App`）→ `write` / `scroll_to` / `set_focus`；**没有任何** `&mut Node` 泄漏路径 |
| I7（harness 默认无 GPU） | 60 项单测全部走 `TestApp` / `NodeTree` / `Window`，无 device |
| I9（应用层不接触平台 API） | `app.rs` / `interaction/**` 里 **0 个 Win32 类型**；`SetCapture`/`SetCursor`/`SetTimer` 只在 `platform/windows/**` |
| I11（逻辑像素 / 物理像素一次换算） | 滚动偏移、拖拽 delta、浮层定位、命中点全部**逻辑像素**；`to_physical_*` 只在 `DamageAccumulator` / 编码器 |
| I17（静态结构 / 动态属性分离） | `focus_ring` 属 `Style`（静态）；`interactive`/`cursor` 属 `StylePatch`（`bind_style` 求值） |
| D15（交互机制内化） | 焦点 / 滚动 / 虚拟列表 / 浮层 / 拖拽 / 动画六件套全在引擎（§8.4 的表） |

---

## 2. 事实基线

### 2.1 已落地代码的形状（本阶段必须遵守，逐条都有出处）

1. `NodeTree::order()` 是 **DFS 先序**（`tree.rs::order_insert`），`hit_test` 逆序 + clip 链（`layout/hit_test.rs`），
   `HitResult { node, local_point, clip_chain }` 已公开在 `layout::hit_test`（F-45）。**浮层在 `order` 末尾 ⇒ 命中优先**（`hit_test.rs:444` 已有此用例）。
2. `place_subtree(node, reference, epoch, force, parent_hidden)` 是**唯一**写 `layout.bounds`（窗口绝对、**内容坐标**）的地方；
   `layout.bounds` 变化时自动 `damage ∪= old ∪ new`、`prev_layout = old`、`clear(REUSABLE)`（P04 §7 / P06 F-60）。
   **P10 起 `place_subtree` 不感知滚动**（A2 修订）：偏移只在 `paint_node` 与 `hit_test::pick` 里累加。
3. lane 内的图元持有**绝对** bounds；复用判据 = `REUSABLE` + lane 存在 + `clip_key` 一致（`paint/pass.rs:176-198`）。
4. `DamageAccumulator::push(bounds, aa_padding, clip: Option<PhysicalRect>)` **自带裁剪**；paint 侧每发一个图元就 `push` 一次
   （`paint/pass.rs:447/473/480`），所以视口外的子树 damage 在 paint 侧已被 clip 裁掉。
5. 节点级 `Node.damage` 由 `touch()` / `place_subtree` 累积，**不带 clip**（P03 §9.4）——这是滚动路径唯一需要补裁剪的地方。
6. `settle_frame` 只清 `LAYOUT|PAINT`；`A11Y` / `ORDER` / `GEOMETRY` 保留（P06 §9.4）。`ORDER` 目前**没有清除者**（P11 消费）。
7. 帧流水：`App::frame` = `take_wake_request` → `needs_frame` → `begin_frame` → `refresh_styles` → `refresh_text` →
   `layout_if_dirty`（仅 `LAYOUT`）→ `layout_text` → `paint_frame` → `commit_frame`（P06 §9.2）。
8. 平台泵：`PeekMessageW` drain → `collect_events`（`App::dispatch` 逐事件）→ `run_frame_hook` → `dispatcher.take_frame_wake()` →
   `frame_all` → `drain_tasks` → `GetMessageW` 阻塞（`event_loop.rs:288-345`）。`wndproc` **不调用 `App`**（只写 `WndProcState`）。
9. `test_support::app::inject` 当前只做 `hit_test`（P06 §10.4），`InputOutcome { hit }` 只有 1 个字段；`test_support/input.rs` 仅此一型。
10. `Node` 尺寸 **1128 B**（P06 预算）/ 上限 1152（`tree.rs::node_size_after_p06`）；`Node` 无「交互」字段。
11. `StylePatch` 已有 `interactive: Option<bool>` 与 `cursor: Option<Cursor>`，**无消费者**（P03 预留，P10 首次消费）。
12. `NodeTree::set_layout_override` 已公开（P04 §10.2），分栏拖拽只差「谁在何时调它」。

### 2.2 本机 probe（`%TEMP%\kg-p10-interaction`，2026-11-13）

> 环境：Windows，**非交互式 agent 会话**；`windows 0.62.2`（与 workspace 同版本）+ `Win32_Graphics_Gdi`（`WNDCLASSW`/`RegisterClassW` 的门控）；
> 独立 `CARGO_TARGET_DIR`；`--offline`（crate 源已在本地 registry 缓存）。入口：`cargo run --offline`。

| # | 实测 | 数值 | 对设计的作用 |
|---|---|------|------|
| 1 | `LoadCursorW` 的 8 个 `IDC_*` | ARROW `0x10003` / HAND `0x1001f` / IBEAM `0x10005` / SIZEWE `0x10011` / SIZENS `0x10013` / NO `0x10017` / SIZEALL `0x10015` / CROSS `0x10009` | §5.4 的 `Cursor → IDC_*` 映射表（全部成功，无 null） |
| 2 | `SetCursor` 往返 | `SetCursor(arrow)` 返回前值 `0x10007`；`GetCursor()` = `0x10003`（相等） | 光标是**线程状态**，可在泵里随时覆盖 |
| 3 | `SetCursor(None)` | `GetCursor()` = `0x0` | 传 `None` = 清空光标（**不可见**），引擎永不这么干 |
| 4 | 类光标 = null，`DefWindowProcW(WM_SETCURSOR, HTCLIENT)` | 返回 **0**，`GetCursor()` = **0x0** | **必须**改类光标或自己设，否则客户区没有光标（A5） |
| 5 | 类光标 = `IDC_ARROW`，同样消息 | 返回 0，但 `GetCursor()` = **arrow** | 默认光标交给系统；引擎只在非 `Default` 时覆盖 |
| 6 | 类光标 = null 的非客户区 | `HTLEFT` → 返回 1，光标 `0x10011`；`HTTOP` → 1，`0x10013`；`HTBOTTOMRIGHT` → 1，`0x1000d`；`HTCAPTION` → 0，`0x10003` | 非客户区**系统自己处理**，引擎只关 `HTCLIENT` |
| 7 | `WM_SETCURSOR` 解码 | `wParam` = hwnd；`LOWORD(lParam)` = `1`（`HTCLIENT`）；`HIWORD(lParam)` = `0x0200`（`WM_MOUSEMOVE`） | §5.5 的判定式 |
| 8 | `SetCapture` / `GetCapture` / `ReleaseCapture` | 隐藏窗口上：`during == hwnd`；`ReleaseCapture` = `Ok(())`；after = `0` | 拖拽捕获可落地、可单测（不需要真实鼠标） |
| 9 | `TrackMouseEvent(TME_LEAVE)` | `Ok(())` | `PointerLeave` 的生产者；无真实鼠标，`WM_MOUSELEAVE` 本身未验证（V-66） |
| 10 | 双击 / 拖拽阈值 | `GetDoubleClickTime()` = **500 ms**；`SM_CXDOUBLECLK` = `SM_CYDOUBLECLK` = **4**；`SM_CXDRAG` = `SM_CYDRAG` = **4**；`SM_MOUSEWHEELPRESENT` = 1；光标 32×32 | `InputThresholds::default()` 的来源；**不硬编码** |
| 11 | `MSG.time` / `GetMessageTime()` | 两者相等（`1090101703`，非零 tick） | 未来做「事件合并」可用；P10 用 `Instant`（测试可注入，§11.6） |
| 12 | `SetTimer(1 ms)` | 320 ms 内 **20** 次 ⇒ 实测周期 **16.0 ms**；`KillTimer` 后 60 ms 内 **0** 次 | 动画 tick 的粒度上限（**60 fps 不保证**，§11.3）；与 P09 F-88 一致 |
| 13 | `SetTimer(16 ms)` | 320 ms 内 **12~13** 次 ⇒ **24.6~26.7 ms** | 否掉「用 16 ms 定时器做 60 fps」（比 1 ms 更慢更抖） |

**probe 结论（写进代码注释的三条）**：①类光标必须是 `IDC_ARROW`；②`HTCLIENT` 的光标归引擎，非客户区归系统；
③动画 tick 用 `SetTimer(1)`，实际 16 ms，**不能**承诺 60 fps。

### 2.3 未验证项（登记，不阻塞）

| 编号 | 未验证 | 后续 |
|---|---|---|
| V-66 | `WM_MOUSELEAVE` 的真实投递（需要真实鼠标移出客户区） | 后续的 L5 实机矩阵；P10 依赖 `TrackMouseEvent` 调用成功 + 状态机单测 |
| V-67 | 拖拽中指针移出窗口/跨屏时 `SetCapture` 的实际投递（无交互桌面） | 同上；单测只覆盖「捕获请求的真值序列」 |
| V-68 | 真实滚轮/触控板的 `WM_MOUSEWHEEL` 高频小增量（±40 等） | 平台的 `/120.0` 已保留小数（`platform/event.rs:117`，P09 已落地），本阶段不改 |
| V-69 | 系统定时器在 60 Hz 显示器以外的抖动（120/144 Hz） | 动画按 `Instant` 插值（与 tick 频率解耦），抖动只影响平滑度 |

---

## 3. 模块结构与边界

### 3.1 文件树与模块根形态

`interaction.rs` 是**公共模块根**（P01 §4.2 的两类模块根之一）：`pub mod <child>;` + 契约类型直接定义在根文件。
它**不是** `mod.rs`，因此 `definition_in_module_root` 不适用（P01 §4.3a 实测）；`pub use` 仍然全仓禁止（M-02），
所以公开路径就是 `kytrix_gui::interaction::{FocusScope, ScrollHandle, …}`（P01 §4.5 已登记）。

```rust
// interaction.rs（逐字骨架）
//! 通用交互机制（设计 §8.4 / §3.6 / §3.7）：Action 分发、焦点、滚动、虚拟列表、浮层、拖拽、动画。

pub mod animation;
pub mod drag;
pub mod focus;
pub mod overlay;
pub mod scroll;
pub mod virtual_list;

// 契约类型（定义在本文件；实现放子模块）：
//   FocusScope / PointerState / ScrollHandle / ScrollState / ScrollConfig / ScrollAxis
//   VirtualList / VirtualListConfig / VirtualListState
//   Overlay / OverlayOptions / OverlayPlacement / AnchorSide / OverlayDismiss
//   DragState / DragInfo / DragPhase / ActionSource / ActionCtx
//   AnimationId / AnimationSpec / AnimatedValue / Easing
//   InputThresholds / NodeInteraction
```

**每个类型全 crate 只有一个 `impl` 块**（`multiple_inherent_impl = "forbid"`）。这条约束的直接后果：**交互的公开方法必须
写进 `app.rs` / `window.rs` / `tree.rs` 里已存在的 impl 块**（不能再开第二个 `impl App`）；子模块只提供 `pub(crate)` 的自由
函数与「类型定义在根文件、impl 唯一地放在子模块」的槽位类型：
`focus::next_focusable(tree, from, backwards)`、`scroll::clamp_offset(slot, viewport)`、`virtual_list::visible_range(…)`、
`overlay::resolve_placement(…)`、`drag::DragSession::update(…)`、`animation::sample(slot, now)`）。

> 这条纪律是 AGENTS §1.3「严禁拆分多个 impl 到不同的文件」+ `multiple_inherent_impl` 的直接后果，也是 P06 的做法
> （`PaintPass` 的实现全在 `paint/pass.rs`，`PaintPass` 的 `impl` 只有一块）。

### 3.2 交互状态归谁（**定死**，避免三处存同一真值）

| 状态 | 存放 | 理由（唯一性） |
|---|---|---|
| 滚动偏移 / 内容尺寸 / 轴 / 虚拟列表槽 / `on_click` / `on_drag` / 浮层槽 | **`Node.interaction: Option<Box<NodeInteraction>>`** | `absolutize` / `paint` / `hit_test` / `Probe` / a11y 全都只有 `&NodeTree` |
| 焦点 | **`Window.focus: FocusScope`** | 每窗口一份；paint 要画环、a11y 要 `TreeUpdate.focus`、键盘要路由 |
| 指针（hover / cursor / press / drag 会话 / capture） | **`Window.pointer: PointerState`** | 与窗口生命周期绑定；`WM_MOUSEMOVE`/`WM_MOUSELEAVE` 是窗口消息 |
| Action handler 表 / KeyBinding 表 / 动画槽 / 时钟 / 阈值 / 事件观测缓冲 | **`App`** | handler 要 `&mut App`（写 `Signal`）；动画要 `&mut StateStore`（写 `Transient`）；阈值来自平台但只影响分发 |

**禁止**：第二份焦点、第二份滚动偏移、第二份「谁在拖拽」。测试用 `Probe` / `Window` 的只读面断言，不另设镜像。

### 3.3 事件分发的唯一入口

```text
平台泵（platform/windows/event_loop.rs）
  └─ collect_events → apply_event → App::dispatch(window, Event)      // 已有
        ├─ 观测缓冲（App::dispatch 内部，容量上限 1024，drop-oldest）
        └─ Event::Input(input) → App::dispatch_input(window, input, now)   // P10 新增（pub(crate)）
              ├─ 命中 → 浮层外部点击 → 焦点 → 拖拽 → 按下 → 抬起/点击 → Action
              ├─ Event::FocusChanged(false) → 清 hover / 取消按下 / 结束拖拽（释放捕获）
              └─ 其它变体：不参与分发（应用从 drain_events 看）
```

`TestApp::inject` 与平台走**同一个** `App::dispatch_input`（唯一实现，杜绝两条路径漂移）。

---

## 4. Action 模型（定死）

### 4.1 `Action`（`src/event.rs`）

```rust
/// 引擎动作身份（`design/phase-01-foundation.md` §4.5 的公开路径：`event::Action`）。
///
/// **身份而非载荷**：同一个 `Action` 可以同时被「点击」「快捷键」「a11y 动作」触达，
/// 三者的差异走 [`ActionCtx::source`]，不进 `Action` 本身（否则不可 `Hash`，也没法做 handler 键）。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// 焦点移到下一个可聚焦节点（Tab 的默认绑定；环内回绕）。
    FocusNext,
    /// 焦点移到上一个（Shift+Tab 的默认绑定）。
    FocusPrev,
    /// 关闭最上层「可关闭」的浮层（Esc 的默认绑定；无浮层时无副作用）。
    Dismiss,
    /// 应用自定义动作：`SmolStr` 是**应用层稳定 key**（建议 `"<模块>.<动词>"`，如 `"chart.resetZoom"`）。
    Custom(SmolStr),
}

impl Action {
    /// 自定义动作的便利构造（`SmolStr::new_static` 走短静态串）。
    #[must_use] pub fn custom(key: &'static str) -> Self;
}
```

**为什么不是纯 `Custom(SmolStr)`**：Tab / Shift+Tab / Esc 是引擎的**内建语义动作**（有默认实现、可被应用覆盖）。
若把它们降级成字符串，应用必须知道引擎的魔术键名，且「覆盖 Tab」变成「注册一个叫 `"focus.next"` 的 handler」——
字符串拼错不报错。枚举变体让覆盖与内建都在类型系统里。

**为什么 `Action` 不带 payload**：`Action` 是 handler 表的键（`Eq + Hash`）。行/列的定位靠
`ActionCtx::node`（声明 `on_click` 的那个节点），应用自建 `NodeId → 业务对象` 映射（§4.4）。

### 4.2 `KeyBinding`

```rust
/// 一条键盘绑定（全应用一份表；快捷键是应用级语义）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyBinding {
    /// 物理键（`Key::Other(vk)` 覆盖字母/数字：`WM_CHAR` 只给文本，键位身份是 vk）。
    pub key: Key,
    /// **精确**修饰键集合（`Ctrl+S` 不匹配 `Ctrl+Shift+S`；未按的修饰键必须为空）。
    pub modifiers: Modifiers,
    /// 命中的动作。
    pub action: Action,
}
```

**匹配规则（定死）**：`event.key == binding.key && event.modifiers == binding.modifiers`，**精确相等**。
先按表内顺序取**第一条**匹配（表很小：默认 3 条 + 应用若干，线性扫描；不做 hash）。

**默认绑定**（`App::new` 写入；`App::unbind_key` 可删、`App::bind_key` 后写覆盖）：

| 绑定 | 动作 | 说明 |
|---|---|---|
| `Tab` + 空修饰键 | `FocusNext` | 引擎内建：焦点循环（§6.3） |
| `Tab` + `SHIFT` | `FocusPrev` | 同上 |
| `Escape` + 空修饰键 | `Dismiss` | 只关浮层；无浮层时是 no-op（demo 仍能从 `drain_events` 看到 Esc） |

**不做**：`KeyBinding` 挂节点（per-node keybinding）、序列绑定（`Ctrl+K Ctrl+C`）、字符键绑定（`Key::Other(vk)` 才是
键位身份，字符输入归 `InputEvent::Text` → 文本 Phase）。

### 4.3 声明侧：`on_click` / `on_drag` / `interactive`

```rust
impl NodeBuilder {
    /// 点击（或 Enter/Space 激活）时声明的动作。冒泡从**被点节点**向上找第一条 `on_click`。
    #[must_use] pub fn on_click(self, action: Action) -> Self;
    /// 可拖拽：按下即开启拖拽会话，移动时按 [`ActionSource::Drag`] 分发该动作。
    #[must_use] pub fn on_drag(self, action: Action) -> Self;
}
```

`NodeBuilder` 把两者塞进 `NodeSpec::with_click/with_drag`，`insert_child` 时写进
`Node.interaction = Some(Box<NodeInteraction { on_click, on_drag, .. })`。

> **命名说明（对齐 brief 的「`NodeBuilder` 的 `on_click`/`on_action`」）**：brief 里的 `on_action` 在本设计里被拆成两半 ——
> 节点侧只**声明动作身份**（`on_click` / `on_drag`），**处理器**统一走 `App::on_action(action, handler)`（§4.4）。
> 理由：节点上挂 `on_action(闭包)` 就是 GPUI 的 listener 形态（每个交互节点一次永久分配、快捷键无法复用同一身份、
> a11y 的 `ActionRequest` 无处映射），而「声明 + 集中注册」让点击与快捷键落到同一个 handler（A1）。

**可交互谓词（唯一定义，`interaction.rs`）**

```rust
/// 该节点是否接受**动作**：声明了动作且未被 `interactive: false` 禁用。
pub(crate) fn accepts_action(node: &Node) -> bool {
    node.style_patch().interactive != Some(false) && node.interaction_actions().is_some()
}
/// 该节点是否可聚焦：接受动作，或被显式标为 `interactive: Some(true)`。
pub(crate) fn focusable(node: &Node) -> bool { accepts_action(node) || node.style_patch().interactive == Some(true) }
/// 该节点是否可成为命中目标（`interactive: false` ⇒ 不作为目标，但子节点照旧）。
pub(crate) fn hittable(node: &Node) -> bool { node.style_patch().interactive != Some(false) }
```

**`interactive` 的三重语义**（与 `paint/style.rs:99` 的注释一致，P10 首次全部落实）：

| 值 | 命中 | 焦点 | 动作 |
|---|---|---|---|
| `None`（默认） | 是（若 `hit_shape`/bounds 命中） | 仅当声明了 `on_click`/`on_drag` | 仅当声明了 |
| `Some(true)` | 是 | **是**（即使没声明动作） | 仅当声明了 |
| `Some(false)` | **否**（自身，不进 `HitResult`） | 否 | 否（冒泡也在此停止） |

### 4.4 handler 注册表 + `ActionCtx`

```rust
/// 动作处理器（应用注册；`FnMut`，可写状态）。
pub type ActionHandler = Box<dyn FnMut(&mut ActionCtx<'_>)>;

/// 一次分发的上下文（**唯一**给应用的交互入口）。
pub struct ActionCtx<'a> {
    app: &'a mut App,
    window: WindowId,
    node: NodeId,             // 声明 on_click/on_drag 的节点（点击/拖拽），或焦点节点（键盘）
    hit: Option<HitResult>,   // 指针事件的原始命中；键盘为 None
    modifiers: Modifiers,
    source: ActionSource,
}

/// 动作来源（把「怎么触发的」从 `Action` 里剥出来）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ActionSource {
    /// 指针点击（或 Enter/Space 激活）：连击计数。
    Click { click_count: u8 },
    /// 键盘绑定命中。
    Key,
    /// 拖拽（按下/移动/抬起）。
    Drag { phase: DragPhase, delta: Point },
    /// 程序调用（`App::dispatch_action`）。
    Programmatic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragPhase { Start, Move, End, Cancel }

impl ActionCtx<'_> {
    #[must_use] pub fn window(&self) -> WindowId;
    #[must_use] pub fn node(&self) -> NodeId;
    #[must_use] pub fn hit(&self) -> Option<HitResult>;
    #[must_use] pub fn modifiers(&self) -> Modifiers;
    #[must_use] pub fn source(&self) -> ActionSource;
    /// **非追踪**读（handler 不是渲染 pass，不登记依赖）。派生值走临时 tracker（读完丢弃）。
    pub fn read<T: Clone + 'static>(&self, signal: &Signal<T>) -> T;
    /// 写状态（`App::write` 的转调：短路 + `dependents` 反查 + 唤醒）。
    pub fn write<T: Clone + PartialEq + 'static>(&mut self, signal: &Signal<T>, value: T) -> bool;
    /// 应用层需要的其它能力（滚动 / 焦点 / 浮层 / 拖拽）走 `&mut App`。
    pub fn app_mut(&mut self) -> &mut App;
}

impl App {
    /// 注册/替换一个动作的处理器（同 `Action` 后写覆盖）。
    pub fn on_action(&mut self, action: Action, handler: impl FnMut(&mut ActionCtx<'_>) + 'static);
    /// 删除处理器（返回是否真的删了）。
    pub fn off_action(&mut self, action: &Action) -> bool;
    /// 程序性分发（消费者：测试、菜单/工具栏的桥、P11 的 a11y `ActionRequest`）。
    pub fn dispatch_action(&mut self, window: WindowId, node: NodeId, action: Action,
                           source: ActionSource) -> bool;
    /// 便捷：以当前焦点节点为 target 分发。
    pub fn dispatch_action_focused(&mut self, window: WindowId, action: Action,
                                   source: ActionSource) -> bool;
    /// 非追踪读（`ActionCtx::read` 的实现；也供应用在 `on_frame` 里取当前值）。
    pub fn read<T: Clone + 'static>(&self, signal: &Signal<T>) -> T;
}
```

**分发算法（逐字，`App::dispatch_action`）**

```text
dispatch_action(window, node, action, source) -> bool:
  // `hit` / `modifiers` 取自窗口的指针状态（`dispatch_input` 在派发前写入 `pointer.last_hit`），
  // 键盘路径的 `hit` 为 None。
  1. if !self.windows.contains_key(&window) { return false }        // 窗口已关：静默
  2. if node 不在该窗口的树里 { return false }                       // 陈旧句柄
  3. // 引擎内建动作：**有 handler 时让 handler 覆盖**（应用可以接管 Tab）
     if let Some(handler) = self.handlers.remove(&action) {
           { let mut ctx = ActionCtx { app: self, window, node, hit, modifiers, source };
             handler(&mut ctx); }
           self.handlers.insert(action, handler);                    // 放回（允许 handler 内重新注册）
           return true
     }
  4. match action {
       FocusNext => focus::move_focus(self, window, /*backwards*/ false),   // §6.3
       FocusPrev => focus::move_focus(self, window, true),
       Dismiss   => overlay::dismiss_top(self, window),                     // §9.3
       Custom(_) => return false,                                          // 未处理：不是错误
     }
     true
```

> `handlers.remove` + 重新 `insert` 的形态与 `App::frame_hook` 的 take/put 一致（P09 S-84），也是唯一允许
> handler 内部再调 `on_action`/`write`/`scroll_to` 的借用编排。**panic 语义**：handler panic 时该 action 的处理器丢失
> （与 `bind_style` 的「闭包丢失」同源，`refresh_styles` 已有对应的自愈分支）。

**未处理的动作不是错误**：`dispatch_action` 返回 `false`。`TestApp` 的 `InputOutcome.action` 因此能区分
「没人要」与「处理了」。debug 构建对 `Custom` 动作**不**打日志（避免高频点击刷屏）。

### 4.5 分发顺序（定死）

```text
指针点击（PointerUp，通过阈值与同节点判定）：
  ① 从命中节点向上冒泡，找第一条 accepts_action 的 on_click（越过 interactive:false 即停）
  ② 用该 Action 走 §4.4（handler 优先，其次引擎内建，否则未处理）
  ③ 没有任何 `on_click` ⇒ 本次点击无动作（焦点已在 PointerDown 时处理）

键盘（Key Down）：
  ① Enter / Space 且存在焦点节点 ⇒ 同「指针点击」的 ①②（source = Click { click_count: 1 }）
  ② 否则查 KeyBinding 表（精确匹配）⇒ Action ⇒ §4.4
  ③ 否则未处理（字符输入 → InputEvent::Text → 文本 Phase）

拖拽（按下/移动/抬起，会话内）：
  ① 会话记录的动作 ⇒ §4.4（source = Drag { phase, delta }）
```

**「焦点→祖先冒泡→全局」**在本设计里的准确含义：①入口是**焦点/命中节点**；②找动作沿**祖先链**；③动作的
**处理器**在**全局**表里（按 `Action` 身份查）。**没有** per-node handler，因此不存在「祖先 handler 与全局 handler 抢同一事件」的二义性。

### 4.6 命中与 `interactive:false`（`layout/hit_test.rs` 的一行改动）

`pick()` 在「子节点都没命中」之后、返回自身之前插入：

```rust
// 设计 §4.9：`interactive == Some(false)` = 不可交互 ⇒ 不作为命中目标。
// 子节点已在上面的循环里独立判定（CSS `pointer-events: none` 的语义：父不可点，子照旧）。
if slot.style_patch().interactive == Some(false) {
    chain.truncate(depth);
    return None;
}
```

这条改动让「禁用按钮」点击穿透到下层（背景/父容器），并让 §4.5 的「冒泡在 `interactive:false` 停止」在
`hit_test` 与 dispatch 两处**语义一致**（一个负责「谁是目标」，一个负责「谁能吃动作」）。

### 4.7 为什么不是 GPUI 的 listener 形态

| GPUI | 本设计 | 原因 |
|---|---|---|
| `on_click(cx.listener(\|this, event, window, cx\| …))`，闭包挂在 element 上、每帧重建 | `on_click(Action)` 声明 + `App::on_action` 注册 | 保留式节点树不重建，闭包挂上去就是一次永久分配；且 GPUI 的 listener 拿 `Entity`/`Window`，会把「写状态」分散到任意位置 |
| `Action` 是 `impl Action` trait + `TypeId` downcast | 引擎枚举 + `Custom(SmolStr)` | 不需要 `Any` downcast，handler 表可以是普通 `HashMap` |
| `cx.notify()` 触发整树重绘 | `write` → `dependents` 反查 → 单节点 `PAINT` | D16 的核心判定（最小失效） |

---

## 5. 事件分发（定死）

### 5.1 逐事件表（`App::dispatch_input`）

> 记号：`tree` = 窗口树，`hit(p)` = `tree.hit_test(p)`，`bubble(n, pred)` = 从 `n` 沿 `parent` 向上第一条满足 `pred` 的节点
> （遇 `interactive: false` 停）。

| 事件 | 处理（顺序即优先级） | 失效（§5.6） |
|---|---|---|
| `PointerDown { position, button, modifiers }` | 1. `hit = hit(position)`<br>2. **浮层外部点击**：若存在 `OverlayDismiss::OutsideClick` 的最上层浮层且 `hit` 不在其子树 ⇒ 关闭它并**返回**（消费）<br>3. **焦点**：`focusable` 的最近命中祖先 ⇒ `set_focus`；否则 `set_focus(None)`<br>4. **拖拽**：`bubble(hit.node, has_on_drag)` 命中 ⇒ 开启会话（记录 origin/action）、请求捕获、**返回**（消费；不再走点击）<br>5. 记录 `press = { node: hit.node, position, button, at: now }` | 焦点变 ⇒ `A11Y`（+可能 `PAINT`）；拖拽 ⇒ 无 |
| `PointerMove { position, modifiers }` | 1. 有拖拽会话 ⇒ 更新 `current`、`dispatch(action, Drag{Move, delta})`<br>2. 否则：`hit = hit(position)`；`hover` 变 ⇒ 重算 `cursor`（§5.4） | 拖拽由 handler 决定（通常写 `LayoutOverride` ⇒ `LAYOUT`） |
| `PointerUp { position, button, modifiers }` | 1. 有拖拽会话 ⇒ `dispatch(action, Drag{End, delta})`、结束会话、释放捕获、返回<br>2. 是 `press` 的同一按钮 + `hit(position).node == press.node` + 位移 ≤ `drag_distance` ⇒ 计算 `click_count`（§5.3）⇒ `bubble(press.node, has_on_click)` ⇒ `dispatch(action, Click{click_count})`<br>3. 清 `press` | 只有 handler 的写入 |
| `PointerLeave` | `hover = None`；`cursor = Default`（**不**结束拖拽：捕获期仍收 move/up）；若 `press` 存在且无捕获 ⇒ 清 `press` | cursor 变化 ⇒ 平台 `SetCursor` |
| `Scroll { position, delta_x, delta_y, modifiers }` | 1. `hit = hit(position)`<br>2. `bubble(hit.node, has_scroll_slot)` ⇒ 该容器；无则丢弃<br>3. 取驱动量（**与轴无关的统一规则**）：`Shift` 按下 ⇒ `delta_x`，否则 ⇒ `delta_y`<br>4. 目标偏移 = `offset - drives * line_height`（`delta > 0` = 滚轮向上 = 内容下移 = offset 减小）<br>5. 注册/重定向平滑滚动动画（§7.5） | 滚动 ⇒ 子树 bounds 平移 + 视口 damage（§7.4） |
| `Key { key, state, modifiers }`，`Down` | 1. `Enter`/`Space` 且 `focused.is_some()` ⇒ 焦点激活（§4.5 的 ①②）；**没找到 `on_click` 就落到第 2 步**（不吞键）<br>2. `KeyBinding` 精确匹配 ⇒ `dispatch(action, Key)`<br>3. 否则未处理 | Tab ⇒ 焦点变（`A11Y`/`PAINT`） |
| `Key`，`Up` | 忽略（无按键状态机；文本编辑另开一条路） | — |
| `Text { text }` | 本阶段**不分发**（文本输入属文本 Phase） | — |

**窗口级事件**（`App::dispatch` 里，不属 `InputEvent`）：

| `Event` | 处理 |
|---|---|
| `FocusChanged { focused: false }` | `hover = None`、`press = None`、结束拖拽（`Drag{phase: Cancel}`）、释放捕获、`cursor = Default` |
| `FocusChanged { focused: true }` | 无（下一次 move 会重建 hover） |
| `CloseRequested` | 无（平台已处理；`App::close_window` 由应用调） |
| `DpiChanged` / `Resized` | 无交互侧动作（尺寸变化会走根 `LAYOUT`，滚动偏移在 `absolutize` 里重新吸收） |
| `Ime` | 文本/IME Phase |

### 5.2 按下 → 抬起的点击判定（定死）

```rust
/// 输入判定的系统阈值（`interaction.rs`；默认值 = 本机 probe 的实测，平台在 `platform_init` 覆盖为系统真值）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputThresholds {
    /// 双击的时间窗口（`GetDoubleClickTime()` = 500 ms）。
    pub double_click_time: Duration,
    /// 双击的位置容差（`SM_CXDOUBLECLK/SM_CYDOUBLECLK` = 4 px）。
    pub double_click_distance: f32,
    /// 「拖动 vs 点击」的位移阈值（`SM_CXDRAG/SM_CYDRAG` = 4 px）。
    pub drag_distance: f32,
}
```

```text
click_count(now, node, position) -> u8:
  if let Some(last) = self.last_click
     && last.node == node
     && now - last.at <= thresholds.double_click_time
     && |position - last.position| <= thresholds.double_click_distance {
        count = last.count.saturating_add(1)
  } else { count = 1 }
  self.last_click = Some(ClickRecord { node, position, at: now, count });
  count
```

`thresholds.double_click_time` 来自 `GetDoubleClickTime()`（probe：500 ms），`double_click_distance` 来自
`SM_CXDOUBLECLK/SM_CYDOUBLECLK`（probe：4 px）。**时间用 `App::clock`（`Instant`）**，测试用 `TestApp::advance` 驱动。

### 5.3 拖拽阈值 / 抑制点击

- 启动：`PointerDown` 命中 `on_drag` 的节点（或祖先）⇒ 会话开始（`DragPhase::Start` 立即分发一次，`delta = 0`）。
- 移动：每次 `PointerMove` 都分发 `Drag{Move, delta}`（`delta = current - origin`，逻辑像素）。**不做节流**——
  平台已按消息合并，且 delta 是纯算术。
- 抑制点击：会话期间或 `|up - down| > thresholds.drag_distance`（probe：`SM_CXDRAG` = 4 px）⇒ `PointerUp` **不**产生
  `Click`。判定用「按下时的节点 + 位移」，不要求抬起点仍在节点内（拖出按钮再松开不该触发点击）。
- 结束：`PointerUp` ⇒ `Drag{End}`；`FocusChanged(false)` / `App::cancel_drag` ⇒ `Drag{Cancel}`。
- 捕获：会话开始 ⇒ `App` 置 `pointer.capture = true`，泵 `SetCapture`；结束 ⇒ `false`，泵 `ReleaseCapture`。

### 5.4 光标解析（`Cursor` → `IDC_*`）

```text
resolve_cursor(window) -> Cursor:
  hover 节点及其祖先里第一条 `style_patch().cursor.is_some()` 的值；都没有 ⇒ Cursor::Default
```

| `Cursor` | Win32 | probe 句柄 |
|---|---|---|
| `Default` | 不调 `SetCursor`（交给类光标 `IDC_ARROW`） | — |
| `Pointer` | `IDC_HAND` | `0x1001f` |
| `Text` | `IDC_IBEAM` | `0x10005` |
| `NotAllowed` | `IDC_NO` | `0x10017` |
| `Grab` / `Grabbing` | `IDC_SIZEALL`（Windows 没有「抓手掌」系统光标；四向移动是惯例） | `0x10015` |
| `ResizeHorizontal` | `IDC_SIZEWE` | `0x10011` |
| `ResizeVertical` | `IDC_SIZENS` | `0x10013` |

**平台侧**（`event_loop.rs` 的泵循环，每轮一次，在 `collect_events` 之后、`frame_all` 之前）：

```rust
// 光标：只在「解析结果变了」时 SetCursor（避免每个 WM_MOUSEMOVE 都进 user32）。
if let Some(cursor) = app.take_cursor_change() {
    let handle = match cursor { Cursor::Default => None, other => Some(load(other)) };
    match handle { Some(h) => SetCursor(Some(h)), None => SetCursor(Some(class_arrow)) };
}
```

- **不加 `WM_SETCURSOR` 分支**：类光标改成 `IDC_ARROW` 后，`DefWindowProcW` 已经处理所有情况（probe 2/5/6）。
- `Cursor::Default` 时显式 `SetCursor(Some(class_arrow))`：否则从 `Pointer` 回到默认会停在手形（`GetCursor` 是线程状态，probe 2/3）。
- `Cursor` 的解析需要 `&NodeTree` + hover，因此放在引擎里；平台只做 `Cursor → HCURSOR` 的映射 + `SetCursor`。

### 5.5 为什么 `clip_chain` 在分发里只做透传

`HitResult.clip_chain` 是「目标被哪些祖先裁剪」的窗口绝对矩形（P04 §12.1）。事件分发**不需要**它来判定
（`hit_test` 已保证点在链内），但 `ActionCtx::hit` 原样带给应用：tooltip 裁剪、拖拽 ghost 的可见范围、
自定义 `Canvas` 的局部坐标都要它。**引擎不消费**它（不新增语义）。

### 5.6 最小失效表（本阶段唯一的「哪些路径标什么」真相）

| 交互 | 写什么 | 直接脏位 | damage | 备注 |
|---|---|---|---|---|
| `set_focus(Some(n))` | `Window.focus` | 旧焦点 / 新焦点：`A11Y`；配了 `focus_ring` 的再 `PAINT` | 仅 ring 节点：`prev ∪ layout`（环在 bounds 外 ⇒ ring 图元自己 push damage，§6.4） | 焦点不变 ⇒ 完全 no-op（不唤醒） |
| 点击 → handler 写 `Signal` | `StateStore` | 按 `dependents` 反查（P03 §14.2） | 读它的节点 | 引擎不额外标脏 |
| 滚轮 / `scroll_to` | `Node.interaction.scroll.offset` | 该容器**子树**：清 `REUSABLE`（**不标 `PAINT`**） | 视口矩形（裁剪后） | 不重算 `bind_style`（§7.6） |
| 拖拽移动 | 应用 handler 写 `LayoutOverride` / `Signal` | `LAYOUT`（`set_layout_override` 内既有）或按信号 | 节点 AABB | 引擎只递 delta |
| 打开浮层 | `tree.append_child(root, …)` | root：`LAYOUT|PAINT|ORDER|A11Y`（`mark_structure`） | 浮层 AABB | 新节点首帧必画 |
| 关闭浮层 | `tree.remove(…)` | 父：`mark_structure` + 旧 AABB 并入 damage | 浮层旧 AABB | P04 的 `remove` 已做 |
| 虚拟列表重开窗 | `append_child` / `remove` | 容器：`LAYOUT|PAINT|ORDER|A11Y` | 容器视口 | 行数变化 ⇒ 内容尺寸显式更新 |
| 动画 tick | `Transient` 值 + 注册节点 `PAINT` | 注册节点 | 注册节点 AABB | 不进依赖图（§3.7） |

**「不唤醒」也是契约**：焦点不变、偏移不变、拖拽 `delta == 0`、动画已结束 ⇒ 一律不置 `wake_requested`
（`TestApp` 用 `take_wake_request()` / `FrameOutcome::Idle` 断言）。

---

## 6. 焦点（`FocusScope`）

### 6.1 存储与单一权威源

```rust
/// 一个窗口的焦点状态（**唯一**权威源：键盘路由、焦点环、a11y 的 `TreeUpdate.focus` 都读它）。
///
/// v1 只有一个作用域（单窗口、无模态对话框）；Tab 序不缓存（`order()` 是 `Vec`，过滤是 O(n)，见 §6.2）。
pub struct FocusScope {
    focused: Option<NodeId>,
}
```

- 存 `Window.focus`；`Window::focus() -> &FocusScope`（只读），`App::{set_focus, focus_next, focus_prev}`（写）。
- **不做**「每个 `FocusScope` 一棵子树」的嵌套作用域（对话框/Tab 组）：v1 单作用域；后续若需要，additive 加
  `FocusScope::root: NodeId` 并用 `hit_test`/祖先链限定。理由：单窗口 v1、无模态对话框（§8.4 的「不做」）。
- 读法：`FocusScope::focused() -> Option<NodeId>`、`FocusScope::is_focused(node) -> bool`。
- 陈旧：写入/读取时都过 `tree.get(node)`；节点被卸载后 `focused` 读到陈旧句柄 ⇒ 视为 `None`（并在 `set_focus` 时清空）。

### 6.2 Tab 序 = `order` 过滤（**不做 `tab_index`**）

```text
next_focusable(tree, from: Option<NodeId>, backwards: bool) -> Option<NodeId>:
  let candidates = tree.order().iter().copied().filter(|n| focusable(tree.get(n)));
  // 从 `from` 在 order 里的位置开始，向一个方向找第一条可聚焦的；到头回绕。
  // `from == None` ⇒ 正向取第一条 / 反向取最后一条。
```

- **为什么不加 `tab_index`**：`order` 是唯一的树序真值（P03 §11.5 的稳定绘制序）；`tab_index` 会引入第二排序，
  与 `ORDER` 脏位、虚拟列表的动态挂载、浮层的末尾插入三处同时冲突。v1 的单窗口表单不需要它；需要时
  additive 加 `Style.tab_index: Option<i32>` + 稳定排序（不破坏 `order`）。
- 顺序 = DFS 先序 = **视觉阅读顺序**（文档流）。浮层的节点在末尾 ⇒ Tab 自然最后进入（符合直觉）。
- 焦点访问过的节点不记忆「最后位置」：Tab 总是从当前焦点继续。

### 6.3 焦点移动与失效

```text
App::set_focus(window, next: Option<NodeId>) -> bool:
  1. 校验：`next` 存在且 `focusable`（否则 `false`，不改状态）
  2. old = focus.focused；两值相等 ⇒ return false（**不标脏、不唤醒**）
  3. focus.focused = next
  4. 标脏：old ⇒ `mark_a11y(old)`；next ⇒ `mark_a11y(next)`
  5. 配了 `Style.focus_ring` 的（old/next）⇒ `mark_paint(node)`（清 `REUSABLE` + damage）
  6. self.wake_requested = true
```

`mark_a11y` 只标 `A11Y`（不产生视觉 damage、不清 `REUSABLE`，P03 §9.3 表第 9 行）；`mark_paint` 才产生 damage。
因此**无 ring 的焦点移动 = 零像素 damage**，`FrameOutcome::Idle`（a11y 更新不提交帧，P06 §9.3 的第二段门禁）——
这条正是「焦点不闪帧」的实现依据。

### 6.4 焦点环（`Style.focus_ring` + paint 在节点 lane 内）

```rust
// paint/style.rs —— 环是**静态结构**（宽/色在挂载时解析）
pub struct FocusRing {
    pub width: f32,   // 环宽（逻辑像素，沿 bounds.outset(offset) 的内侧展开）
    pub offset: f32,  // 相对节点 border box 的外扩
    pub color: Color,
}

// node/builder.rs —— 挂到**节点**上（不进 `Style`：`Style` 是布局/绘制结构，环属交互态）
impl NodeBuilder {
    /// `Some` 时该节点获得焦点后由 paint 在它自己的 lane 内画一圈。
    pub fn focus_ring(self, ring: FocusRing) -> Self;
}
```

> **实现修正（2026-11-13，落地时）**：`focus_ring` 最初放进 `Style`，会让仓库里 40+ 处
> `Style { inner, clip }` 字面量全部失效（含 P02/P04/P06 的测试）。落地改为 `NodeInteraction.focus_ring` +
> `NodeBuilder::focus_ring(..)`：环是**交互态**，不是布局/绘制样式，语义上也更贴（DEVIATIONS F-101）。

**paint 规则**（`paint/pass.rs::rebuild_lane`，在内容之后、clip 后缀之前）：

```text
if let Some(ring) = style.focus_ring && Some(node) == focused {
    let bounds = self.bounds.outset(ring.offset);          // 环在节点外侧
    let radius = spec.corner_radius.map(|r| r + ring.offset);
    push(Quad { bounds, corner_radius: radius,
                paint: Solid { fill: TRANSPARENT, border: Some(Border { width: ring.width, color: ring.color }) } });
    damage.push(bounds, 1.0, self.clip);                    // 环超出 node_damage，必须自己 push
}
```

- **在节点自己的 lane 内**：lane 自带祖先 clip 前缀（P06 §5.4）⇒ 环被滚动容器/父容器正确裁剪；
  不需要第二条 lane 通道，也不需要「焦点环 z 序」的额外语义。
- **环超出节点 bounds** 时 damage 由 ring 图元自己 `push`（`paint/pass.rs` 的既有模式：每个图元 push 一次）。
- 焦点移动 ⇒ `mark_paint(old)` + `mark_paint(new)` ⇒ 两条 lane 重建、两个 damage（old∪new 的 bounds + ring 外扩）。
- **不做**「焦点环由应用 `bind_style` 画」：那要应用自己知道焦点（`FocusScope` 不是 `Signal`，读不到；
  若做成 `Signal<Option<NodeId>>` 就又出现第二份焦点真值）。

### 6.5 与 a11y 的接口（P11 消费，本阶段只留）

| P11 需要 | P10 提供 | 语义 |
|---|---|---|
| `TreeUpdate.focus`（每次更新必带，§7.5） | `Window::focus().focused()` | 单一权威源；`None` ⇒ a11y 退化到窗口根（P11 决定） |
| 焦点变化的增量 | `DirtyFlags::A11Y`（旧 + 新） | 焦点移动只标 `A11Y`；P11 的 pass 收集 `A11Y` 节点 |
| `Focus` 动作（a11y 请求把焦点给某节点） | `App::set_focus(window, Some(node))` | 校验 `focusable`，走同一失效路径 |
| `Click` 动作（a11y 请求激活） | `App::dispatch_action(window, node, action, ActionSource::Programmatic)`；或合成 `PointerDown/Up`（§7.7 的映射） | 本设计选**前者**：合成指针事件会经过拖拽/双击/浮层关闭等副作用，a11y 的 `Click` 只该激活声明 |
| 可聚焦节点的集合 | §6.2 的 `focusable` 谓词（`pub(crate)`；P11 在 `a11y` 模块内用它填 `add_action(Focus)`） | 与 Tab 序同源 |

### 6.6 键盘路由（与 §4.5 一致）

| 键 | 谁处理 |
|---|---|
| `Tab` / `Shift+Tab` | `KeyBinding`（默认绑定）→ `FocusNext` / `FocusPrev` |
| `Enter` / `Space` | **先**焦点激活（冒泡 `on_click`）；没有声明 ⇒ 落回 `KeyBinding` 表（应用可绑） |
| `Escape` | `KeyBinding` → `Dismiss`（关浮层）；无浮层 ⇒ no-op，事件仍可从 `drain_events` 看到 |
| `↑/↓/Home/End/PageUp/PageDown` | **不绑定**（列表滚动属应用；引擎不接管） |
| 其它 | `KeyBinding` 表；未命中 ⇒ 未处理 |

---

## 7. 滚动（`ScrollHandle`，**paint 期偏移**）

> **上级裁决（2026-11-13）**：**不**把偏移吸收进 `layout.bounds`（那会每帧重排 + 重建整棵子树，违背 §3.7 / D16 与 R-1）。
> 偏移是 `Transient`，只在 **paint** 与 **hit_test** 两处应用；滚动不写脏位、不跑 taffy、不跑 `refresh_styles`。

### 7.1 偏移存哪（三条约束的交点）

```rust
pub(crate) struct ScrollSlot {
    /// 当前偏移（逻辑像素，已 clamp；**内容坐标 → 屏幕坐标**的位移量）。
    offset: Point,
    content: Size,
    axis: ScrollAxis,
    line_height: f32,
}
```

- 存 `Node.interaction.scroll.offset`（`Point`），**不进** `StateStore`：`paint` / `hit_test` / `Probe` / a11y 只有 `&NodeTree`
  （DEVIATIONS F-94 的结论不变；上级裁决只改了「谁应用偏移」，没改存储位置）。
- **`Transient` 语义**：`App::scroll_to` 不写 `Signal`、不 `mark_*`、不置 `PAINT`/`LAYOUT`；`layout.bounds`、`bind_style`、
  taffy 缓存**全部不变**。
- `layout.bounds` 恒为**内容坐标**（滚动无关）：`absolutize` 里**没有** `content_shift`，这一点是 A2 修订后的硬约束。

### 7.2 paint 的累计偏移（唯一的图元位移点）

```text
paint_node(node, offset: Point, clip, hidden):
  let screen_offset = offset                      // 祖先滚动容器的累计位移
  // ① 复用判据多一项：lane 的偏移戳必须与本次一致
  reusable = REUSABLE && clip_key 一致 && tree.lane_offset(node) == Some(screen_offset)
  // ② 重建时：所有图元 bounds / clip 矩形按 -screen_offset 平移
  rebuild_lane(node, offset):
      bounds        = node.layout().bounds.translate(-offset)
      clip.push(padding_box.translate(-offset))
      every primitive: bounds/rect/position 都用 translate(-offset)
      tree.set_lane_offset(node, offset)
  // ③ 进子树时：容器若挂了 scroll 槽，子节点的 offset 再加它自己的偏移
  child_offset = offset + node.scroll.map_or(ZERO, |s| s.offset)
  for child in children/hoisted: paint_node(child, child_offset, …)
```

- **clip 的坐标空间**：滚动容器自己的 padding box 属于**屏幕空间**（容器不动），所以它用 `offset`（祖先累计）平移；
  它**子树内部**的裁剪容器用 `child_offset` 平移。两段各自正确，互不污染。
- **`hit_test` 用同一份算术**（§7.3）：屏幕点 ↔ 内容点的换算只有一个方向（进入容器子树时 `point + offset`）。
- **`Probe::bounds`** 返回内容坐标（布局事实）；需要屏幕坐标时用 `TestApp::probe().hit(...)` 或帧记录里的 primitives
  bounds（后者是屏幕坐标）。

### 7.3 命中测试随偏移

```text
pick(tree, node, point: Point /* 屏幕 */, offset: Point, chain):
  let screen_bounds = node.layout().bounds.translate(-offset);
  // 自身判定 / local_point 都用 screen_bounds
  // clip（Clip::Bounds）用 screen_bounds.inset(border) 判定与入链
  let child_offset = offset + node.scroll_offset;
  for child: pick(tree, child, point, child_offset, chain)
```

`HitResult.local_point` 仍是「目标节点 border box 局部坐标」（与 P04 契约一致），因此滚动后命中同一点会得到相同
`local_point`（内容跟着手指走）——这正是单测 P10-16 的断言。

### 7.4 `scroll_to`（不标脏、不重排）

```text
App::scroll_to(&mut self, handle: &ScrollHandle, offset: Point) -> Result<(), EngineError>:
  1. 校验：窗口存在、节点存活且挂了 scroll 槽；否则 StaleNode / NotScrollable
  2. viewport = node.bounds.inset(node.border)                    // 屏幕空间（容器不动）
     max = content - viewport.size（每轴 clamp 到 >= 0）
     next = clamp(offset)（只 clamp 该容器的轴）
  3. if next == current { return Ok(()) }                         // 不唤醒、不产生 damage
  4. slot.offset = next
  5. window.request_damage(viewport)                              // **唯一**的 damage 来源（视口矩形）
  6. 若该容器挂了虚拟列表 ⇒ virtual_list::rewindow(self, list)    // 结构变化才标 ORDER/LAYOUT（§8.3）
  7. self.wake_requested = true
```

- **不调用** `place_subtree` / `compute_layout` / `mark_paint`；`FrameIndex` 正常推进（一帧 = 一次提交）。
- `needs_frame` 把 `pending_damage.is_some()` 也算进去（§7.6），因此「滚动但无其它脏位」也会提交一帧。

### 7.5 视口 / 内容 / 范围（显式 `content`）

```rust
pub struct ScrollConfig { pub axis: ScrollAxis, pub line_height: f32 }   // line_height 默认 48.0 逻辑像素/行
pub enum ScrollAxis { Vertical, Horizontal }                             // v1：单轴
pub struct ScrollState { pub offset: Point, pub content: Size, pub viewport: Size, pub axis: ScrollAxis }
impl App {
    /// 让一个节点成为滚动容器。**要求** `Style.clip == Clip::Bounds`（否则 `NotScrollable`）。
    pub fn make_scroll(&mut self, window: WindowId, node: NodeId, config: ScrollConfig)
        -> Result<ScrollHandle, EngineError>;
    /// 内容尺寸（滚动范围来源）。虚拟列表由引擎算；普通容器由应用显式给。
    pub fn set_scroll_content(&mut self, &ScrollHandle, content: Size) -> Result<(), EngineError>;
    pub fn scroll_to(&mut self, &ScrollHandle, offset: Point) -> Result<(), EngineError>;
    pub fn scroll_by(&mut self, &ScrollHandle, delta: Point) -> Result<(), EngineError>;
    pub fn scroll_state(&self, &ScrollHandle) -> Option<ScrollState>;
}
```

- **内容尺寸为什么显式**：taffy 没有「可滚动内容盒」（布局盒恒为视口大小，子节点只是「溢出」在同一个坐标系里），
  由应用 / 虚拟列表给 `content` 是唯一无歧义的口径。
- `viewport` 由 `bounds.inset(border)` 现算（布局变化后自动正确）；`content` 默认 = `viewport`（不可滚）。

### 7.6 lane 复用语义与**代价**（DEVIATIONS F-99，必须记录）

上级要求「lane 内容不重建、仅重写实例（或按需重建视口内节点）」。本引擎落到**后者**，根因是 lane 的坐标空间不可分离：

| 事实 | 后果 |
|---|---|
| lane 内图元持**绝对** bounds（P06 契约，`paint/pass.rs`） | 滚动后旧 lane 的像素位置是错的 |
| lane 头部嵌**祖先 clip 前缀**（P06 §5.4），矩形在**窗口空间** | 给 lane 加单个平移，会把「窗口空间的视口 clip」和「内容空间的内容图元」一起移 |
| 反向做法（把外层 clip 反算进内容空间） | 需要 `Scene` 增加 per-lane 平移 + 编码器对 8 类 `Primitive` 逐个平移（含 `GlyphRun` 的逐字形位置），
并保证 `GpuPrimitive`/golden 不变 —— 属 P06/P07 契约级改动，超出 P10 的边界 |

**因此的实现（记录在 DEVIATIONS F-99）**：paint 发现 `Node.lane_offset != 当前累计 offset` ⇒ **重建该 lane**（图元按新偏移生成，
不重算 `bind_style`、不跑 taffy）；damage **只**来自视口矩形（不来自节点 damage）。
代价上界 = 本次滚动容器子树里**已挂载**的节点数（虚拟列表下 = 视口内行数 + overscan）；未虚拟化的超长内容仍是 O(n)（§17 R-1）。

### 7.7 浮层里的滚动

浮层是 root 的子节点，它内部的滚动容器走**完全相同**的 `paint_node` 路径（`child_offset` 从浮层节点继续累加）。
`dismiss == OutsideClick` 的判定用 `hit_test`（已随偏移换算）⇒ 滚过之后点浮层内容命中自动正确。

### 7.8 damage 与帧推进

- `Window.pending_damage: Option<Rect>`（逻辑像素，取并集）；`paint_frame` 第一步 `damage.push(rect, 1.0, None)` 后清空。
- `needs_frame` 增加 `|| pending_damage.is_some()`。
- 滚动**不**清 `REUSABLE`、**不**标 `PAINT`：脏位表里没有滚动这一行（与 §3.4 行 7 的「只改 uniform / clip」语义一致，
  只是「uniform」在 CPU 侧的等价物是「重建该 lane 的图元」）。

---
## 8. 虚拟列表（`VirtualList`）

### 8.1 结构（三个角色，全部现有节点种类）

```text
container（应用建）   : Clip::Bounds + position: relative，make_virtual_list 时挂 scroll 槽 + virtual 槽
  ├── row i（引擎挂） : position: absolute, left:0, right:0, top: i*extent, height: extent
  │     └── …         : 应用在 build(usize) 里给的任意子树（正常流内布局）
  └── row j（引擎挂） : …
```

- **没有 sizer 节点**：`content` 尺寸由 `count × extent` 直接写进 scroll 槽（§7.4），不占节点。
- 行是**绝对定位**（不是 `LayoutOverride`）：绝对定位节点仍是正常容器，可挂子节点并跑 taffy；
  `LayoutOverride` 是布局叶子（P04 §10.3 表：不能挂子节点）⇒ 不能当行（A4）。
- 行的 `left/right` 同时给 ⇒ 宽度自动拉伸到容器内容宽（taffy 的 absolute + 双 inset 求宽）。行高 = `extent`（均匀）。
- `make_virtual_list` **要求** `position: relative`（绝对子节点的包含块是最近定位祖先；否则行会挂到窗口根）。
  不满足 ⇒ `EngineError::NotVirtualList { node }`（与 `NotScrollable` 分开，便于应用定位问题）。

### 8.2 可见窗口计算（纯函数，可单测）

```text
visible_range(offset, viewport, extent, count, overscan) -> Range<usize>:
  if extent <= 0 || count == 0 { return 0..0 }
  first = floor(offset / extent).saturating_sub(overscan)
  last  = ceil((offset + viewport) / extent) + overscan          // 半开区间
  first..min(last, count)（last 至少 first+1，避免 0 高度视口）
```

**只支持均匀行高**（`extent`）。变高行需要「先测量再定位」，会把 measure 反馈接回交互层（taffy 的 measure 是纯函数，
不参与回收），留后续 Phase 的「变高虚拟列表」——v1 明确不做（§1.2）。

### 8.3 挂载 / 回收 API

```rust
pub struct VirtualListConfig { pub extent: f32, pub overscan: usize }
pub struct VirtualList { window: WindowId, node: NodeId }        // `Copy`
pub struct VirtualListState { pub count: usize, pub first: usize, pub last: usize,
                              pub mounted: usize, pub offset: Point, pub viewport: Size }

impl App {
    pub fn make_virtual_list(&mut self, window: WindowId, container: NodeId, config: VirtualListConfig)
        -> Result<VirtualList, EngineError>;
    /// 设置条目总数与行工厂；只在**新建行**时调用 `build`（复用的行不重建）。
    pub fn set_virtual_items(&mut self, list: &VirtualList, count: usize,
                             build: impl FnMut(usize) -> NodeBuilder + 'static)
        -> Result<(), EngineError>;
    pub fn virtual_list_state(&self, list: &VirtualList) -> Option<VirtualListState>;
    /// P11 的 a11y：行节点 → 条目下标（`pos_in_set` 的来源）。
    pub fn virtual_row_index(&self, list: &VirtualList, row: NodeId) -> Option<usize>;
}
```

**`set_virtual_items` / 重开窗的逐字流程**

```text
rewindow(list):
  1. state = visible_range(offset, viewport, extent, count, overscan)
  2. 回收：mounted 里 index ∉ state ⇒ tree.remove(row)（`remove` 自己会把旧 AABB 并入父 damage、释放 lane）
  3. 挂载：state 里没有行的 index ⇒
        let builder = take_virtual_builder(container)          // 闭包在槽里，借用编排同 bind_style 的 take/put
        let row = builder(index).build(tree, container)?;       // 应用的行子树
        put_virtual_builder(container, builder)
        tree.set_virtual_row(row, /*top*/ index as f32 * extent, extent)   // 绝对定位（§8.1）
        mounted.insert(index, row)
  4. content = Size(viewport.width, count * extent)  → scroll 槽（clamp 现有 offset）
  5. 行数/顺序变化 ⇒ `mark_structure(container)`（`LAYOUT|PAINT|ORDER|A11Y`）
       —— 只有「集合成员变了」时才标（range 不变的重开窗是 no-op）
  6. self.wake_requested = true（若有挂载/回收）
```

**脏位语义（定死）**

| 变化 | 脏位 | 说明 |
|---|---|---|
| 滚动（offset 变） | **无脏位**；paint 因 `lane_offset` 失配重建该子树 lane；视口 damage | §7.6 / §7.8；**不**标 `ORDER`、**不**标 `PAINT`/`LAYOUT` |
| 重开窗（挂载/回收） | 容器 `LAYOUT|PAINT|ORDER|A11Y` | `ORDER` = 子集合变了；`A11Y` = 可见子集变了（P11） |
| `count` 变（同一窗口内） | 内容尺寸 + 容器 `LAYOUT|PAINT|ORDER|A11Y`（若窗口内行数也变） | 滚动范围跟着变 |
| 行内容变化（应用写 `Signal`） | 正常 `PAINT`（按 `dependents`） | 与虚拟列表无关 |

### 8.4 回收与 `NodeId` 代际（stale 的三种来源）

| 场景 | 引擎行为 | 应用必须怎么做 |
|---|---|---|
| 应用缓存了被回收行的 `NodeId` | 该 `NodeId` 的代际在 `Arena::free` 时递增；再访问 ⇒ `EngineError::StaleNode`（P03 §6.3） | **不缓存行 `NodeId`**；用 `virtual_row_index` 反查或自建 `index → 业务数据` |
| 应用缓存了 `VirtualList` / `ScrollHandle` | 两者带 `window + node`；节点卸载后 → `StaleNode`；窗口关闭后 → 静默 `Ok`/`None`（窗口不存在不是错误） | 窗口关闭时丢弃全部句柄 |
| 回收后同槽位被新行复用 | 新行拿到**不同**代际的 `NodeId`（`Arena` 语义） | 同上；虚假命中在单测里被钉死（P10-23） |

### 8.5 a11y 可见子集接口（P11 消费）

| 接口 | 用途 |
|---|---|
| `container` 节点 + `DirtyFlags::A11Y` | 列表容器重发（`Role::List`、`children`） |
| `App::virtual_list_state(list)` | `count`（`set_size`）、`first..last`（可见窗口） |
| `App::virtual_row_index(list, row)` | 行 → `pos_in_set`（1-based） |
| 行节点的 `A11Y` 脏位 | 挂载/回收时由 `mark_structure` 置位 |
| **不暴露** | 「未挂载的行」——a11y 树里不存在（与视觉一致；主设计 §7.4「虚拟列表按可见范围生成子节点」） |

### 8.6 与 `Probe` / 测试

`Probe` 不变（`bounds` / `hit` / `lane_of` 已够）。测试断言用：`virtual_list_state().mounted`、
`probe().count(Selector::Id("row"))`、`probe().bounds(row)`（含滚动平移）。

---

## 9. 浮层（`Overlay`）

### 9.1 层级 = `order` 末尾（无 `z-index`）

- `App::create_overlay` ⇒ `tree.append_child(root, node)` ⇒ 节点是 root 的最后一个子节点 ⇒ `order` 的最后一个
  DFS 子树 ⇒ 绘制最后、`hit_test` 逆序时**第一个**被测（`hit_test.rs:444` 的既有用例就是按此契约写的）。
- 多浮层：创建顺序即层级（后创建在上）。`App::bring_overlay_to_front(&overlay)` = `append_child(root, node)`
  的重挂（`append_child` 已支持重挂并标 `mark_subtree_paint`，P03 §9.3 补记 1）。
- **不做**：浮层嵌套（浮层里的菜单再开浮层 = 第二个 root 子节点）；模态遮罩（对话框属后续 Phase）；`z-index` 数值。

### 9.2 定位

```rust
pub struct OverlayOptions { pub placement: OverlayPlacement, pub dismiss: OverlayDismiss }
pub enum OverlayPlacement {
    /// 窗口根的逻辑像素点（左上角）。
    Point(Point),
    /// 锚定到某节点的某一侧（打开时求值一次）。
    Anchor { node: NodeId, side: AnchorSide, gap: f32 },
}
pub enum AnchorSide { Below, Above, Left, Right }
pub enum OverlayDismiss { OutsideClick, Explicit }
pub struct Overlay { window: WindowId, node: NodeId }    // `Copy`
```

`resolve_placement(tree, placement) -> Rect`：`Anchor` 读锚点 `bounds`（含裁剪后的可见部分：用
`hit_test` 的 clip 链语义取 `bounds ∩ clip_chain`），在指定侧放 `gap`，**不**自动翻转（v1 不做 collision
avoidance；超出窗口就超出，应用自己用 `place_overlay` 修正）。浮层坐标写成 taffy 的
`position: Absolute` + `inset { left, top }`，尺寸由内容决定（`auto`）。

```rust
impl App {
    pub fn create_overlay(&mut self, window: WindowId, options: OverlayOptions) -> Result<Overlay, EngineError>;
    pub fn close_overlay(&mut self, overlay: &Overlay) -> bool;
    pub fn place_overlay(&mut self, overlay: &Overlay, placement: OverlayPlacement) -> bool;
    /// 把浮层提到最上层（`append_child(root, node)` 的重挂）。
    pub fn bring_overlay_to_front(&mut self, overlay: &Overlay) -> bool;
    pub fn overlays(&self, window: WindowId) -> impl Iterator<Item = (NodeId, OverlayDismiss)> + '_;
}
```

### 9.3 点击外部关闭（**唯一**的消费点）

```text
on PointerDown（§5.1 的第 2 步，**在焦点之前**）:
  for overlay in root.children.rev().filter(has_overlay_slot) {   // 自上而下
      if overlay.overlay_dismiss != OutsideClick { continue }
      if hit.map(|h| is_in_subtree(hit.node, overlay)).unwrap_or(false) { break }   // 点在它里面 ⇒ 停
      close_overlay(overlay); return /* 消费：不移动焦点、不派发动作 */;
  }
```

- **消费**是关键：否则「点菜单外的按钮」会同时关菜单 + 触发按钮（标准行为是关闭并吃掉这次点击）。
- `Explicit` 的浮层不参与；`Escape`/`Dismiss` 动作可以关任意最上层浮层（包括 `Explicit`）。
- 关闭 ⇒ `tree.remove(node)` ⇒ 父 `mark_structure` + 旧 AABB 并入 damage（P04 §8.2 的 `remove` 已实现）。

### 9.4 clip 链

浮层是 root 的直接子节点 ⇒ `hit_test` 的 `clip_chain` 只有 root 自己的 clip（`Clip::None`）⇒ **不会被调用点的
容器裁剪**（`clip_chain` 空）。这正是「浮层必须能超出父容器边界」的要求；反之，浮层**内部**的滚动容器/裁剪容器
照常工作（子树的 clip 链正常累积）。

### 9.5 生命周期与 damage

| 动作 | 脏位/damage |
|---|---|
| `create_overlay` | root `LAYOUT|PAINT|ORDER|A11Y`；浮层自己的 AABB 进 damage（首帧） |
| `place_overlay`（换位置） | `LAYOUT|PAINT`（`set_style` 语义）+ 新 AABB；旧位置由 `place_subtree` 的 `prev ∪ new` 覆盖 |
| `close_overlay` | `remove` ⇒ 父 `mark_structure` + 旧 AABB |
| 浮层内滚动 | §7.7 |

---

## 10. 拖拽（`DragState`）+ `LayoutOverride` 分栏

### 10.1 会话状态机

```text
PointerDown(hit, 拖拽目标)  ⇒ Session { node, action, origin, current }  + capture 请求 + Drag{Start, 0}
PointerMove                 ⇒ current = position; Drag{Move, current - origin}
PointerUp                   ⇒ Drag{End, current - origin}; 结束会话; release capture
FocusChanged(false)/cancel  ⇒ Drag{Cancel}; 结束会话; release capture
```

- 会话的「拖拽目标节点」= 冒泡得到的第一条 `on_drag` 节点（不一定是命中节点：抓手可以是子节点）。
- **不节流**：每条 `PointerMove` 都分发（平台已合并；delta 是纯算术）。应用若嫌频繁可在 handler 里自己降频。
- `App::drag_state(window) -> Option<DragState>`（`Copy` 句柄：`{ window, node }`）、
  `App::drag(&DragState) -> Option<DragInfo>`（`{ origin, current, delta, phase }`）、`App::cancel_drag(&DragState)`。
- 引擎**不**改任何 `LayoutOverride`（A4/裁决 8）：分栏矩形由应用的 handler 算。

### 10.2 阈值 / 抑制点击 / 捕获

- 抑制点击：会话存在 **或** `|up - down| > thresholds.drag_distance`（probe：`SM_CXDRAG` = 4 px）⇒ 无 `Click`
  （§5.3）。
- 捕获：`Window.pointer.capture: bool`，泵按 `App::take_capture_request() -> Option<bool>` 调
  `SetCapture`/`ReleaseCapture`（probe：隐藏窗口上也成功）。
- 拖拽期间的光标：走 `StylePatch.cursor`（应用绑 `ResizeHorizontal` / `Grabbing`）；引擎不硬编码。

### 10.3 分栏（逐字示例，写进 `interaction/drag.rs` 的 rustdoc）

```rust
// 应用层：左侧面板 + 6px 抓手 + 右侧面板（都用 LayoutOverride：几何由应用算）
let left = NodeBuilder::boxed()
    .layout_override(Rect::new(0.0, 0.0, 320.0, 600.0))
    .build(tree, root)?;
let grip = NodeBuilder::boxed()
    .layout_override(Rect::new(320.0, 0.0, 6.0, 600.0))
    .on_drag(Action::custom("split.resize"))
    .build(tree, root)?;
let right = NodeBuilder::boxed()
    .layout_override(Rect::new(326.0, 0.0, 474.0, 600.0))
    .build(tree, root)?;

app.on_action(Action::custom("split.resize"), move |cx| {
    let ActionSource::Drag { phase, delta } = cx.source() else { return };
    if phase == DragPhase::End { return }                    // 最后一帧已是最新
    let split = (320.0 + delta.x).clamp(160.0, 480.0);       // 应用的夹紧规则
    let window = cx.window();
    let Some(target) = cx.app_mut().window_mut(window) else { return };
    let tree = target.tree_mut();
    // 三次写入：左 / 抓手 / 右。`set_layout_override` 内部标 `LAYOUT|PAINT` 并清 `node..root` 缓存（P04 §10.2）
    let _ = tree.set_layout_override(left, Some(LayoutOverride::new(Rect::new(0.0, 0.0, split, 600.0))));
    let _ = tree.set_layout_override(grip, Some(LayoutOverride::new(Rect::new(split, 0.0, 6.0, 600.0))));
    let _ = tree.set_layout_override(right, Some(LayoutOverride::new(Rect::new(split + 6.0, 0.0, 794.0 - split, 600.0))));
});
```

> **为什么不写 `Signal`**：`LayoutOverride` 是**结构**（P04 §10.3 把它归为 `Style` 之外的独立逃生口），
> 不经过 `bind_style`，所以「拖拽 → 改 override」只能在 handler 里直接调 `set_layout_override`。
> 写 `Signal` 再在 `on_frame` 里消费也可行，但多一跳且绕（示例只给最短路径）。

### 10.4 与浮层/焦点的交互

| 场景 | 行为 |
|---|---|
| 拖拽中开始另一处按下 | 忽略（会话已存在；`PointerDown` 在会话期只更新 press 记录） |
| 拖拽中窗口失焦 | `Drag{Cancel}` + 释放捕获 + 清 press（§5.1 的 `FocusChanged(false)`） |
| 拖拽目标的节点被卸载 | 下一次 `PointerMove`/`Up` 校验 `tree.get` 失败 ⇒ 静默结束会话（`Drag{Cancel}`） |
| 拖拽与点击 | 阈值内仍是点击（分栏抓手的 4 px 抖动不会丢点击） |

---

## 11. 动画（`AnimationClock`）

### 11.1 存储与注册

```rust
/// 动画句柄（`Copy`；`slot` 带代际，取消/结束后旧句柄失效）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AnimationId { window: WindowId, index: u32, generation: u32 }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Easing { Linear, EaseOutCubic }

pub struct AnimationSpec { pub duration: Duration, pub easing: Easing }

/// 被动画驱动的值（`Transient` 是唯一落点：不进依赖图，`bind_style` 用 `cx.read_transient` 读）。
pub enum AnimatedValue {
    Scalar { target: Transient<f32>, to: f32 },
    Color { target: Transient<Color>, to: Color },
}

impl App {
    /// 注册一个动画：起点 = `target` 的当前值；`nodes` 是**显式注册**的「每帧要重绘的节点集合」。
    /// 已有同 `target` 的动画 ⇒ 替换（起点取当前值）。
    pub fn animate(&mut self, window: WindowId, value: AnimatedValue, nodes: &[NodeId],
                   spec: AnimationSpec) -> Result<AnimationId, EngineError>;
    /// 取消（幂等；返回是否真的取消了）。抖动/悬停切换用。
    pub fn cancel_animation(&mut self, id: &AnimationId) -> bool;
    /// 该窗口是否还有未完成的动画（消费者：测试；平台泵用 `has_animations`）。
    pub fn animations_running(&self, window: WindowId) -> bool;
    /// 全部窗口是否还有未完成的动画（消费者：平台泵决定 `SetTimer(1)` / `KillTimer`）。
    pub(crate) fn has_animations(&self) -> bool;
}
```

- **为什么槽在 `App`**：tick 要同时 `&mut StateStore`（写 `Transient`）与 `&mut NodeTree`（`mark_paint`），
  只有 `App` 能同时借到两者；放 `Window` 就得把 store 传进 window（P03 §5.1 的借用纪律不允许）。
- **为什么值是 `Transient`**：`bind_style` 只能用 `ReadCtx::{read, read_transient}` 读值。进度若做成 `Signal<T>`，
  每帧一次写 = 每帧一次 `dependents` 反查 + 唤醒（§3.7 明确禁止）；`Transient` 写**不失效**，重绘由
  `nodes` 集合的 `mark_paint` 显式给出。
- `nodes` 显式注册的语义 = §3.7 的「写 `PAINT` 到订阅该动画的**节点集合**（显式注册）」。

### 11.2 推进（`App::frame` 的插入点）

```text
App::frame(window):
  1. if !windows.contains(window) → None
  2. let wake = take_wake_request()
  3. self.tick_animations(window)        // ← P10 新增：写 transient + mark_paint(注册节点)
  4. if !window.needs_frame(wake) → record_idle + Idle        // 动画在跑时上一步必然留下 PAINT
  5. begin_frame → styles → text → layout → layout_text → paint → commit
```

```text
tick_animations(window):
  for slot in animations where slot.window == window:
      t = ((now - slot.start) / slot.duration).clamp(0, 1)
      v = ease(t)
      match slot.kind {
          Scalar { target, from, to } => store.set_transient(target, lerp(from, to, v)),
          Color  { target, from, to } => store.set_transient(target, lerp_color(from, to, v)),
          Scroll { handle, from, to } => scroll_to(handle, lerp_point(from, to, v)),   // §7.5
      }
      for node in slot.nodes { tree.mark_paint(node) }        // 只清 REUSABLE + damage，不标 PAINT 语义求值
      if t >= 1.0 { 写终值; 删槽 }
```

- `now` = `App::clock`（§11.6）。**时长 0** 的动画：注册时直接写终值 + 不建槽。
- 颜色插值走 `Color` 的既有插值（P02 `ColorResolver` 的 sRGB/linear 路径）——v1 用 `Color::lerp`（sRGB 空间线性插值，
  与 `ColorResolver` 的 gamma 口径一致；OKLab 插值留主题 Phase）。

### 11.3 平台定时器接口（`event_loop.rs`）

```text
ANIMATION_TIMER_ID: usize = 1;

每轮循环（§5.4 的光标之后就绪处）：
  if app.has_animations() && timer_id.is_none() {
      timer_id = Some(SetTimer(Some(hwnd), ANIMATION_TIMER_ID, 1, None));   // probe: 实测 16.0 ms
  } else if !app.has_animations() && timer_id.is_some() {
      KillTimer(Some(hwnd), timer_id.take());                              // probe: 之后 0 次 WM_TIMER
  }

wndproc:
  WM_TIMER 且 wParam == ANIMATION_TIMER_ID ⇒ state.wake()                  // 只唤醒，不直接推进
```

- **定时器不推进动画**，只 `dispatcher.wake()`；推进发生在 `App::frame` 的 tick（因此「一帧最多 tick 一次」由帧门禁保证，
  定时器抖动只影响帧的时刻）。
- 帧率上限 = 16 ms（probe）。**不承诺 60 fps**；动画按 `Instant` 插值，因此 tick 抖动只影响平滑度不影响终值（V-69）。
- `Application::run` 退出时 `KillTimer`（`shutdown` 一步，P09 §5.4 已有时序）。

### 11.4 动画类型与缓动

| 类型 | 用途 | 备注 |
|---|---|---|
| `Scalar` | 展开高度、透明度、进度 | `lerp(f32)` |
| `Color` | 选中高亮、禁用过渡 | `Color::lerp`（sRGB 线性；OKLab 换插值空间属主题 Phase） |
| `Scroll`（内部） | 滚轮平滑滚动（§7.5） | 走 `scroll_to`，与程序化滚动同一路径 |

`Easing::{Linear, EaseOutCubic}`：`EaseOutCubic(t) = 1 - (1-t)^3`。**断言终值精确等于 `to`**（`t = 1.0` 时直接返回 `to`，
不靠浮点收敛）。

### 11.5 结束回 `Idle`（I3 的闭环）

```text
最后一个动画结束 ⇒ tick 删槽 ⇒ has_animations() == false
  ⇒ 泵 KillTimer（probe：之后 60 ms 内 0 次 WM_TIMER）
  ⇒ 下一轮 frame：needs_frame(wake=false) == false ⇒ FrameOutcome::Idle，FrameIndex 不变
```

单测断言：`advance(到结束)` + `drain()` 若干次后，再 `drain()` 得到 `FrameOutcome::Idle` 且 `frame_index()` 不再增长（P10-60）。

### 11.6 时钟（确定性测试的**唯一**手段）

```rust
pub struct App { /* … */ clock: Instant }         // 生产 = Instant::now()；测试 = TestApp 控制
impl App {
    /// 平台泵每轮调用一次（`event_loop.rs`），把时钟推进到真实时间。
    pub(crate) fn sync_clock(&mut self);          // self.clock = Instant::now()
}
impl TestApp {
    /// 推进测试时钟（双击/动画/拖拽时间全用它；**不**改任何状态）。
    pub fn advance(&mut self, by: Duration);
}
```

- **不引入 `Clock` trait / `Arc<dyn Fn() -> Instant>`**：只有「生产」与「测试」两种来源，一个 `Instant` 字段 + 一个
  `pub(crate) fn` 就够；trait 会把「时钟」变成第三种可插拔抽象（AGENTS §1.2）。
- `App::dispatch` / `App::dispatch_input` 读 `self.clock`（不再各自 `Instant::now()`），因此测试完全确定。

---

## 12. 公开面 / 死代码 / 规范

### 12.1 公开路径表增量（接 P01 §4.5 / P03 §4.1 / P04 §4.1 / P06 §4.1）

| 类型 | 公开路径 | 定义文件 |
|---|---|---|
| `Action` `KeyBinding` `ActionHandler` `ActionSource` `InputOutcome` | `kytrix_gui::event::*` | `src/event.rs` |
| `FocusScope` `ScrollHandle` `ScrollState` `ScrollConfig` `ScrollAxis` | `kytrix_gui::interaction::*` | `src/interaction.rs` |
| `VirtualList` `VirtualListConfig` `VirtualListState` | `kytrix_gui::interaction::*` | `src/interaction.rs` |
| `Overlay` `OverlayOptions` `OverlayPlacement` `AnchorSide` `OverlayDismiss` | `kytrix_gui::interaction::*` | `src/interaction.rs` |
| `DragState` `DragInfo` `DragPhase` `ActionCtx` | `kytrix_gui::interaction::*` | `src/interaction.rs` |
| `AnimationId` `AnimationSpec` `AnimatedValue` `Easing` | `kytrix_gui::interaction::*` | `src/interaction.rs` |
| `InputThresholds` | `kytrix_gui::interaction::InputThresholds` | `src/interaction.rs` |
| `FocusRing` | `kytrix_gui::paint::style::FocusRing` | `src/paint/style.rs` |

**不新增公开**：`Window::hwnd()`（I9）、`App::on_event(closure)`（S-82 明确不做事件回调）、`App::tick()`（帧由 `frame` 驱动）、
`NodeTree::set_scroll_offset`（走 `App::scroll_to`）、`Cursor → HCURSOR` 的映射（平台私有）。

### 12.2 每个新增 `pub(crate)` item 的**非 test 消费者**（dead_code 审计表）

| item | 定义 | 非 test 消费者 |
|---|---|---|
| `NodeInteraction`（`on_click`/`on_drag`/`scroll`/`virtual`/`overlay`） | `interaction.rs` | `node/tree.rs`（字段）、`interaction/**`、`app.rs::dispatch_input` |
| `ScrollSlot` / `scroll::clamp_offset` / `scroll::content_shift` | `interaction/scroll.rs` | `node/tree.rs::place_subtree`、`interaction/virtual_list.rs` |
| `focus::{next_focusable, move_focus, mark_focus_change}` | `interaction/focus.rs` | `App::dispatch_action`、`interaction/overlay.rs`（Tab 在浮层内） |
| `virtual_list::{visible_range, rewindow}` | `interaction/virtual_list.rs` | `App::set_virtual_items`、`App::scroll_to` |
| `overlay::{resolve_placement, dismiss_top, outside_click_target}` | `interaction/overlay.rs` | `App::{create_overlay, dispatch_input}` |
| `DragSession` / `drag::begin/update/end` | `interaction/drag.rs` | `App::dispatch_input` |
| `AnimationSlot` / `animation::{sample, lerp_color}` | `interaction/animation.rs` | `App::{animate, tick_animations}` |
| `PointerState` | `interaction.rs` | `window.rs`、`App::dispatch_input`、平台泵 |
| `NodeTree::{set_virtual_row, reset_layout_stats, scroll_slot, action_chain}` | `node/tree.rs` | `interaction/**`、`app.rs` |
| `App::{sync_clock, take_cursor_change, take_capture_request, has_animations}` | `app.rs` | `platform/windows/event_loop.rs` |
| `PlatformEvent::PointerLeave` | `platform/event.rs` | `event_loop.rs`（`WM_MOUSELEAVE`） |
| `PlatformEvent::` 指针事件的 `modifiers` | `platform/event.rs` | `event_loop.rs::apply_event` |
| `Window::{focus, pointer, pointer_mut}` | `window.rs` | `App::dispatch_input`、`paint/pass.rs`（focused） |

> 纪律（P06/P09 已确立）：**新增的每个 `pub(crate)` 必须在表里有非 test 消费者**，否则
> `cargo clippy --all-targets -D warnings` 的 `dead_code` 会拒。设计期就按这张表写代码，不允许「先加再删」。

### 12.3 明确不加 / 规范

- **不加** `#[allow(...)]` / `#[expect(...)]`（AGENTS §4；P09 的审计口径是 0 处）。
- `unsafe` **只在** `platform/windows/**`（`SetCapture`/`ReleaseCapture`/`SetCursor`/`LoadCursorW`/`SetTimer`/`KillTimer`/
  `TrackMouseEvent`），每处带 `// SAFETY:`（句柄来源、生命期、线程 = 主线程）。
- `app.rs` / `interaction/**` / `event.rs` 里 **0 个 Win32 类型**（I9）；`Cursor` 是引擎类型，映射表在平台层。
- 单测内联 `#[cfg(test)] mod tests`（AGENTS §5.1，与 DEVIATIONS A-02 一致）；不建 `interaction/tests.rs`。
- `interaction.rs` 是**公共模块根**，允许定义类型（P01 §4.3a 实测）；但**每个类型只有一个 `impl` 块**
  （`multiple_inherent_impl = "forbid"`），且交互方法写进 `app.rs`/`window.rs`/`tree.rs` 里**已有的** `impl` 块。

### 12.4 `Node` 尺寸预算

| 字段 | 字节 |
|---|---|
| `interaction: Option<Box<NodeInteraction>>` | **+8** |
| 合计（P06 的 1128 + 8） | **1136 ≤ 1152** |

`NodeInteraction` 的内容（`on_click: Option<Action>` 24 + `on_drag: Option<Action>` 24 + `scroll: Option<ScrollSlot>` +
`virtual_list: Option<VirtualSlot>` + `overlay: Option<OverlaySlot>`）全在堆上，**不进 `Node`**。

### 12.5 demo 增量（可选，`crates/kytrix_gui_demo`）

P09 的 demo 只有三个色块 + 一行文本（点色块改色、Esc 关窗）。P10 **可选**追加三样，用来人工验收引擎侧交互
（不新增 crate、不新增依赖；demo 仍不进 `default-members`）：

| 增量 | 形态 | 验收点 |
|---|---|---|
| 点击 → Action | 三个色块改 `on_click(Action::custom("swatch.select"))`；`App::on_action` 里写 `active` 信号 + 写剪贴板（替换 P09 在 `on_frame` 里手写 `slot_at` 的命中逻辑） | 点击命中 → 只有该色块 `PAINT`；`InputOutcome.action` 命中 |
| 滚动 | 右侧加一个 `make_scroll` 容器（`Clip::Bounds` + 30 行文本），`set_scroll_content` 给内容高度 | 滚轮 → 内容平滑滚动到目标；damage 只覆盖视口；结束后回 `Idle`（CPU 不空转） |
| 焦点 | 三个色块 `Style.focus_ring` + `interactive: Some(true)`；Tab/Shift+Tab 循环 | 焦点环跟随 Tab；焦点移动**不**产生像素 damage（无 ring 的兄弟块不动） |

`--snapshot` 路径不变（若加了滚动，快照仍取 `offset = 0` 的首帧）。
`node_size_after_p10` 断言 `<= 1152` 并把 P10 的预算写成注释（1136）。

---

## 13. 错误面（`EngineError` 增量）

```rust
// src/error.rs（追加两个变体，强类型、无字符串泛化）
/// 目标节点不是滚动容器（未 `make_scroll`，或 `Style.clip != Clip::Bounds`）。
NotScrollable { node: NodeId },
/// 目标节点不是虚拟列表容器（未 `make_virtual_list`，或不是 `Clip::Bounds + position: relative`）。
NotVirtualList { node: NodeId },
```

复用既有变体：`StaleNode { node }`（陈旧 `NodeId`/`ScrollHandle`/`VirtualList`/`Overlay`/`DragState`）。
**不新增**：`UnknownAction`（未处理动作返回 `false`，是正常路径）、`Interaction(String)`（违反强类型要求）。

| 生产点 | 变体 |
|---|---|
| `App::make_scroll` / `scroll_to` / `scroll_by` / `set_scroll_content` | `NotScrollable` / `StaleNode` |
| `App::make_virtual_list` / `set_virtual_items` | `NotVirtualList` / `NotScrollable` / `StaleNode` |
| `App::create_overlay` / `place_overlay` / `close_overlay` | `StaleNode`（窗口不存在 ⇒ 静默） |
| `App::{set_focus, animate, cancel_animation, drag*}` | `StaleNode`（窗口不存在 ⇒ 静默） |

---

## 14. 测试清单（实际落地 **66** 项内联单测 + **7** 项 `--features test-support` 端到端）

### 14.1 `interaction/focus.rs`（8）

| # | 用例 | 断言 |
|---|---|---|
| 1 | `tab_order_is_dfs_preorder` | 三兄弟 + 一个孙节点 ⇒ Tab 序 = `order()` 过滤 |
| 2 | `tab_skips_plain_nodes` | 无 `on_click` 且 `interactive: None` 的节点不在序里 |
| 3 | `tab_wraps_both_directions` | 最后一个 → 第一个；第一个 → 最后一个 |
| 4 | `focus_with_no_candidates_is_none` | 空树 / 全不可聚焦 ⇒ `set_focus` 返回 false，`focused()` = None |
| 5 | `focus_change_marks_a11y_only` | 无 ring：`A11Y` 在旧/新，`!LAYOUT && !PAINT`，damage 空 |
| 6 | `focus_ring_marks_paint_and_damage` | 配 `Style.focus_ring`：两节点 `PAINT` + `!REUSABLE`，damage = 两者 AABB |
| 7 | `disabled_node_is_not_focusable` | `interactive: Some(false)` ⇒ 不进 Tab 序；`hit_test` 也不返回它 |
| 8 | `refocusing_same_node_is_a_noop` | `set_focus(x)` 两次 ⇒ 第二次 false、脏位不变、`!take_wake_request()` |

### 14.2 `interaction/scroll.rs`（10）

| # | 用例 | 断言 |
|---|---|---|
| 9 | `make_scroll_requires_bounds_clip` | `Clip::None` ⇒ `NotScrollable` |
| 10 | `offset_clamps_to_content_minus_viewport` | 越界两端都夹到 `[0, max]`；`max == 0` 时恒 0 |
| 11 | `unchanged_offset_is_a_noop` | 同值 ⇒ 无 damage、无唤醒、`FrameOutcome::Idle` |
| 12 | `scroll_does_not_touch_layout_or_dirty` | `layout.bounds` 逐位不变；`REUSABLE` 仍在；无 `PAINT`/`LAYOUT`；`bind_style` 调用计数 0 |
| 13 | `scroll_shifts_primitives_by_offset` | 滚 N ⇒ 子树 `RenderedFrame.primitives[].bounds` 各平移 N（容器自身图元不动） |
| 14 | `scroll_damage_is_the_viewport` | `FrameRecord.damage` ⊆ 视口（物理像素）；节点 damage 不参与 |
| 15 | `scroll_advances_frame_index` | 连续两次滚动各提交一帧（`FrameIndex` +1，非 `Idle`） |
| 16 | `hit_test_follows_scroll_offset` | 滚动后：旧屏幕点不再命中子节点，新屏幕点命中且 `local_point` 不变 |
| 17 | `nested_scrolls_accumulate` | 内外各滚 ⇒ 孙节点 primitives 偏移 = 两者之和 |
| 18 | `wheel_target_is_nearest_scroll_ancestor` | 深处节点的滚轮 ⇒ 最内层容器；无容器 ⇒ 无变化 |
### 14.3 `interaction/virtual_list.rs`（7）

| # | 用例 | 断言 |
|---|---|---|
| 19 | `visible_range_math` | `offset=0, viewport=400, extent=100, overscan=0` ⇒ `0..4`（含端点规则） |
| 20 | `overscan_expands_both_ends` | `overscan=2` ⇒ 首尾各多 2（夹到 `[0, count]`） |
| 21 | `mounts_only_visible_rows` | 100 条 / 视口 400 ⇒ 挂载 4 行 |
| 22 | `scroll_rewindows_and_recycles` | 滚 250（视口 400）⇒ 挂载 `2..7`（行 2…6）；行 0/1 被 `remove` |
| 23 | `recycled_node_id_is_stale` | 回收行的旧 `NodeId` ⇒ `tree.get` 为 `StaleNode`；新行 id 不同 |
| 24 | `content_size_is_count_times_extent` | `set_virtual_items(100)` ⇒ `scroll_state.content.height == 100*extent`，clamp 生效 |
| 25 | `rows_are_absolute_and_follow_scroll` | 行 `inset.top == i*extent`（布局）；滚动后行的 primitives `y` 相应减小、`layout.bounds` 不变 |

### 14.4 `interaction/overlay.rs`（7）

| # | 用例 | 断言 |
|---|---|---|
| 26 | `open_appends_last_root_child` | `order().last()` 属于浮层子树 |
| 27 | `hit_test_prefers_overlay` | 同点既有底层节点又有浮层 ⇒ `HitResult.node` 在浮层内 |
| 28 | `outside_click_closes_topmost_and_consumes` | 关闭 + 返回的 `InputOutcome.action == None` + 焦点未变 |
| 29 | `inside_click_keeps_overlay` | 浮层内按下 ⇒ 不关闭，动作照常分发 |
| 30 | `close_adds_old_bounds_to_damage` | 关闭后父节点 damage ⊇ 浮层旧 AABB |
| 31 | `stacked_overlays_close_one_at_a_time` | 两层：第一次点外面只关最上层；第二次关下一层 |
| 32 | `explicit_dismiss_ignores_outside_click` | `OverlayDismiss::Explicit` ⇒ 点外面不关；`Dismiss` 动作能关 |

### 14.5 `interaction/drag.rs`（5）

| # | 用例 | 断言 |
|---|---|---|
| 33 | `drag_begins_on_declared_node` | `on_drag` 节点按下 ⇒ `drag_state` Some，`capture == true`，`Drag{Start}` 分发 |
| 34 | `move_dispatches_delta` | 移动 (10,4) ⇒ handler 收到 `delta == (10,4)` |
| 35 | `threshold_suppresses_click` | 位移 1 px ⇒ 仍有点击；5 px ⇒ 无 `Click`，有 `Drag{End}` |
| 36 | `end_releases_capture` | 抬起 ⇒ `drag_state` None、`capture == false` |
| 37 | `drag_on_plain_node_does_nothing` | 无 `on_drag` ⇒ 无会话、无捕获 |

### 14.6 `interaction/animation.rs`（8）

| # | 用例 | 断言 |
|---|---|---|
| 38 | `scalar_starts_at_from` | 注册瞬间 transient == 当前值；`t=0` tick 后仍为 from |
| 39 | `scalar_midpoint_linear` | `Linear` + 1 s：`advance(500ms)` ⇒ 值 == `(from+to)/2` |
| 40 | `ease_out_cubic_is_monotonic_and_exact` | 采样单调；`t=1` ⇒ **精确** `to` |
| 41 | `finish_removes_slot` | 结束后 `animations_running(window) == false`；再 tick 不改值 |
| 42 | `tick_marks_paint_only_on_registered_nodes` | 两个节点，只注册一个 ⇒ 只有它 `!REUSABLE` |
| 43 | `retarget_replaces_slot_from_current` | 动画中途重新 `animate` ⇒ 槽数 1，起点 = 中途值 |
| 44 | `color_animation_interpolates` | `Color::lerp` 端点 + 中点（容差 1e-3） |
| 45 | `scroll_animation_lands_on_clamped_target` | 目标超出范围 ⇒ 终值 = clamp 后的值 |

### 14.7 `event.rs`（5）

| # | 用例 | 断言 |
|---|---|---|
| 46 | `action_identity_is_hashable` | `Custom("a") != Custom("b")`；引擎变体互不相等；可作 `HashMap` 键 |
| 47 | `key_binding_modifiers_are_exact` | `Ctrl+S` 不匹配 `Ctrl+Shift+S`，也不匹配空修饰 |
| 48 | `key_binding_distinguishes_other_vk` | `Key::Other(0x41)` 与 `Key::Other(0x42)` 不共享绑定 |
| 49 | `input_outcome_default_is_unhandled` | `Default` ⇒ `action == None`、`target == None`、`click_count == 0` |
| 50 | `pointer_events_carry_modifiers` | `PointerDown { modifiers: SHIFT }` 的往返相等 |

### 14.8 `app.rs` / dispatch（5）

| # | 用例 | 断言 |
|---|---|---|
| 51 | `enter_activates_focused_and_bubbles` | 焦点在子节点、`on_click` 在父 ⇒ 父的动作被分发，`target == 父` |
| 52 | `binding_dispatches_before_default` | 绑 `Ctrl+K` → `Custom` ⇒ handler 收到 `ActionSource::Key` |
| 53 | `unhandled_action_returns_false` | 无 handler 的 `Custom` ⇒ `false`，无脏位 |
| 54 | `engine_actions_need_no_handler` | `FocusNext`/`Dismiss` 无 handler 时仍执行 |
| 55 | `handler_can_reregister_and_read_state` | handler 内 `on_action`/`write`/`read` 都生效；`read` 读到刚写的值 |

### 14.9 `test_support`（5，仅 `--features test-support`）

| # | 用例 | 断言 |
|---|---|---|
| 56 | `inject_click_reports_action_and_minimal_damage` | `InputOutcome{action, target, click_count == 1}`；`dirty_nodes` 只含目标 |
| 57 | `inject_scroll_produces_viewport_damage` | `ScrollState.offset` 变；`FrameRecord.damage` ⊆ 视口（物理像素） |
| 58 | `inject_tab_moves_focus_and_wraps` | Tab 两次 ⇒ 焦点按 `order` 前进、末尾回绕；`focus()` 与 `dirty_nodes` 的 `A11Y` 一致 |
| 59 | `inject_double_click_reports_count_two` | `advance(200ms)` 内两次点击同点 ⇒ 第二次 `click_count == 2`；`advance(600ms)` 后回到 1 |
| 60 | `animation_advance_reaches_idle` | `advance` + `drain` 到结束 ⇒ `FrameOutcome::Idle` 且 `frame_index()` 不再增长 |

> 落地结果：`cargo test -p kytrix_gui` **491 → 557**（+66）；`--features test-support` **574**（含 `p10_tests` 端到端）
> + 集成 5；demo 1 项。评审后追加 11 项回归用例（未知窗口、滚动命中、浮层消费、陈旧焦点、虚拟列表首帧/resize、
> 嵌套滚动 damage、按键修饰键、动画重定向、拖拽阈值/二次按下、陈旧滚动句柄、焦点环外扩）。表中编号保留规划口径，
> 落地差异见 DEVIATIONS §15.4 / §15.6。

---

## 15. 验收门禁

```powershell
cargo test -p kytrix_gui                                   # 491 → 551（+60）
cargo test -p kytrix_gui --features test-support            # 500+5 → 560+5
cargo +nightly clippy --all-targets --all-features -- -D warnings
cargo +nightly fmt --all --check
cargo doc -p kytrix_gui --no-deps
cargo build -p kytrix_gui --release                        # panic=abort 下 debug_assert 分支不参与编译
cargo check -p kytrix_gui --target x86_64-unknown-linux-gnu   # 环境既有约束：stub pkg-config（V-04）
cargo run -p kytrix_gui_demo                               # 人工：滚动/点击/焦点
```

| 门禁 | 判定 |
|---|---|
| 无 `#[allow(` / `#[expect(` / `#[path]` / `pub use` / `mod.rs` | 全仓 0 处（P09 口径） |
| `unsafe` 只在 `platform/**` | `grep -c "unsafe" app.rs event.rs interaction/*.rs interaction/*/*.rs` = 0 |
| `Node` 尺寸 | `node_size_after_p10` ≤ 1152（实际 1136） |
| 空闲零帧 | 动画结束后 `FrameOutcome::Idle` 且 `frame_index()` 不变 |
| 最小失效 | `inject_click` 的 `dirty_nodes` 只含被点节点；滚动不触发 `bind_style` |

---

## 16. 需裁决项（5 项，含建议）

| # | 议题 | 建议 | 替代与代价 |
|---|------|------|-----------|
| **A1** | `Action` 的形态：引擎枚举 + `Custom(SmolStr)` + 集中 handler；`ActionCtx` 带 `{window, node, hit, modifiers, source}` | **采纳**（§4） | ①只做 `Custom`：Tab/Esc 的默认行为无法类型化覆盖；②节点挂闭包（GPUI 形态）：每节点 alloc + 快捷键无法复用身份 + a11y `ActionRequest` 无映射 |
| **A2** | 滚动偏移吸收进 `layout.bounds`（`place_subtree` 的 `content_shift`），滚动 = 重跑子树 `place_subtree`；damage 裁到视口 | **采纳**（§7） | ①paint 期平移：`hit_test`/`Probe`/a11y bounds/damage 四处各加一次偏移；②per-lane 变换：祖先 clip 会被一起平移（clip 在窗口空间）；③`Transient` 双写：两个真值 |
| **A3** | 焦点：`Window` 持有 `FocusScope`；Tab 序 = `order` 过滤；焦点环 = `Style.focus_ring` 画在节点 lane 内；不做 `tab_index` | **采纳**（§6） | ①`tab_index`：第二排序与 `ORDER` 冲突；②独立 overlay lane：绕过 clip（浮层外的环可见）；③焦点存 `App`：a11y/paint 多一次查表 |
| **A4** | 虚拟列表：容器 = `Clip::Bounds + position:relative`，行 = 绝对定位（非 `LayoutOverride`），均匀行高 + overscan，`content` 显式 | **采纳**（§8） | ①`LayoutOverride` 行：布局叶子不能挂子节点；②自动内容尺寸：taffy 无可滚动内容盒；③全量重建：丢 `bind_style` 依赖与 lane 复用 |
| **A5** | 平台三处 + 不加 `WM_SETCURSOR` 分支：类光标 `IDC_ARROW`、`SetCapture`/`ReleaseCapture`、`SetTimer(1)`+`WM_TIMER`；光标由泵按解析结果 `SetCursor` | **采纳**（§5.4 / §11.3） | ①保留 null 类光标 + 自己处理 `WM_SETCURSOR`：probe 证明 null 类光标下 `HTCLIENT` 是空光标，还得为非客户区补一套；②每个 `WM_MOUSEMOVE` 都 `SetCursor`：每帧一次 user32 调用；③`SetTimer(16)`：实测 26.7 ms（比 1 ms 更差） |

---

## 17. 风险与待复核

| # | 风险 | 处置 / 触发条件 |
|---|---|---|
| **R-1** | **滚动与 lane 复用**：paint 因 `lane_offset` 失配重建被滚动子树的 lane（图元重新生成，不跑 taffy/样式）。未虚拟化的超长内容（10k 节点）每帧 O(n) | ①damage 只来自视口，不会退化成全帧；②「大内容必须配 `VirtualList`」写进 rustdoc；③若 L4 基准证明不可接受 → additive 做「per-lane 平移 + 编码器逐图元平移」（要动 `Scene`/`GpuPrimitive` 契约与三套 golden，见 F-99） |
| **R-2** | **`place_subtree` 的 `force` 重入**：滚动重跑子树，若与 `layout_if_dirty` 同帧交错会重复统计 | ①滚动只发生在帧外（`dispatch_input`），帧内不调；②`scroll_to` 末尾 `reset_layout_stats()`（§7.8）；③单测 `absolutize_preserves_applied_offset` 钉死坐标一致 |
| **R-3** | **浮层与 damage**：浮层在 `order` 末尾 ⇒ 每次开/关都碰 root 的 `mark_structure`（root 的 `LAYOUT` 可能牵动整棵树） | ①root 是 `size: 100%`（`root_style_is_percent_100`），`depends_on_children == false` ⇒ 冒泡在 root 停止（P03 §9.3 表「固定宽高」行）；②浮层用 `position: absolute`（出流）⇒ `affects_parent == false`，root 不会因浮层尺寸变化而重排 |
| **R-4** | **动画帧率**：`SetTimer(1)` 实测 16.0 ms、`SetTimer(16)` 26.7 ms（probe）⇒ 60 fps 不保证 | ①按 `Instant` 插值 ⇒ 抖动只影响平滑度；②`EaseOutCubic` 的端点精确；③V-69 登记；若不可接受 → 后续 Phase 换 `DwmFlush`/`WaitForVBlank` 驱动（不改 `AnimationClock` 契约） |
| **R-5** | **虚拟列表回收导致 stale**：应用缓存行 `NodeId` | ①代际语义（§8.4）+ 单测 P10-23；②API 只暴露 `VirtualList`（不带行 id）与 `virtual_row_index`；③文档写明「行 id 不跨帧有效」 |
| **R-6** | **Action 生命周期**：`Custom(SmolStr)` 拼写错误静默 no-op；handler panic 后处理器丢失 | ①`Action::custom("…")` 用 `SmolStr::new_static`；②`dispatch_action` 返回 `bool` ⇒ 测试可断言「必须处理」；③panic 丢 handler 与 `bind_style` 丢闭包同源，debug 下 `refresh_styles` 的自愈分支已有先例（仅记录，不额外兜底） |
| **R-7** | **`interactive: false` 的命中语义变化**（P04 的 `hit_test` 行为改变） | P04 的单测里若有「禁用节点可命中」的断言会失败 → 逐条核对；这是设计 §4.9 的既定语义，属修正而非回归 |
| **R-8** | **捕获/光标在真实输入下的行为**（V-66/V-67）：单测只覆盖状态机 | 后续的 L5 实机矩阵；`SetCapture`/`SetCursor` 已在隐藏窗口验证调用成功（probe 8/2） |
| **R-9** | **`InputEvent` 加 `modifiers` 的兼容面**：demo / `test_support` / P09 的单测构造点要同步 | 一次性改完（`grep "PointerDown {"`）；`Cargo.lock` 零变化 |
| **V-65** | `ORDER` 脏位在 P10 仍无清除者（`settle_frame` 不清），会一直累积到 P11 | 位是 `pub const`（不触发 dead_code）；`VirtualList`/`Overlay` 单测断言其被置位即可 |
| **V-70** | 浮层**不做** collision avoidance（超出窗口不翻转） | 应用用 `place_overlay` 自行修正；tooltip 需要时再加 |
| **V-71** | 拖拽**不做**节流与速度采样 ⇒ 无「抛掷」惯性（只有滚轮的缓动） | 触控板/惯性滚动属后续 Phase；`AnimationClock` 已能承载 |
| **V-72** | 焦点环颜色不在 `StylePatch` 里 ⇒ 主题切换需重挂载 | 主题 Phase 的批量失效会覆盖（把 `focus_ring.color` 接到 `ThemeHandle.role(...)`） |

---

## 18. 本阶段追加到 `design/DEVIATIONS.md` 的条目（§十五）

| 编号 | 内容 |
|---|---|
| **F-93** | `Action` 用引擎枚举 + `Custom(SmolStr)`（`event::Action`），**不是** `TypeId` downcast 的 trait 动作；handler 表键必须有 `Eq + Hash`。`ActionSource` 承载「点击/键盘/拖拽/程序」的差异 |
| **F-94** | **滚动偏移不进 `StateStore` 的 `Transient`**（设计 §3.7/§8.4 的原文），改存 `Node.interaction.scroll.offset`：`place_subtree`/`paint`/`hit_test`/`Probe` 只有 `&NodeTree`，存 store 必须给这些函数加参数且必然出现第二份真值 |
| **F-95** | **滚动吸收进绝对布局盒**（`place_subtree` 的 `content_shift`），代价 = 移动子树的 lane 重建；设计 §3.4 行 7「滚动偏移只改 uniform / clip，不改 lane 内容」在本引擎的 lane 模型（lane 内含绝对 bounds + 祖先 clip 前缀）下不可实现，除非给 lane 加「内容变换」并让 clip 留在窗口空间（两套坐标） |
| **F-96** | **probe：null 类光标 + `HTCLIENT` 的光标是 null（不可见）**；非客户区（`HTLEFT`/`HTTOP`/`HTBOTTOMRIGHT`）由 `DefWindowProcW` 自行设尺寸光标并返回 1；`HTCAPTION` 返回 0 但设箭头。因此 P10 把类光标改为 `IDC_ARROW` 并**不加** `WM_SETCURSOR` 分支（§2.2 的 probe 表 4/5/6） |
| **F-97** | `GetDoubleClickTime()` = 500 ms / `SM_CXDOUBLECLK` = 4 / `SM_CXDRAG` = 4（probe）⇒ `InputThresholds::default()` 用这些值，并由平台在 `platform_init` 写入系统真值（`app.rs` 不出现 Win32） |
| **S-86** | 引擎侧交互状态的三处归属（**唯一真值**）：节点槽 → `Node.interaction`；焦点/指针 → `Window`；handler/绑定/动画/时钟/阈值 → `App`（§3.2） |
| **S-87** | `ScrollHandle`/`VirtualList`/`Overlay`/`DragState`/`AnimationId` 全部**带 `WindowId`**（`NodeId` 只在单棵树里有意义，跨窗口会假命中）；不匹配 ⇒ `StaleNode` |
| **S-88** | Tab 序 = `order` 过滤，**不做 `tab_index`**（§6.2）；焦点环画在焦点节点自己的 lane 内（§6.4），因此自动受祖先 clip 约束 |
| **S-89** | 浮层 = root 的末尾子节点（无 `z-index`）；点击外部只关**最上层** `OutsideClick` 浮层并**消费**该次按下；`Escape`/`Dismiss` 可关任意层（§9） |
| **S-90** | 虚拟列表：容器 `Clip::Bounds + position: relative`，行绝对定位（`left/right/top/height`），`content = count*extent` 显式；均匀行高 v1 唯一形态（§8） |
| **S-91** | 动画槽在 `App`（tick 需同时 `&mut StateStore` + `&mut NodeTree`）；值落 `Transient`，重绘由**显式注册的节点集合**的 `mark_paint` 给出；平台用 `SetTimer(1)`+`WM_TIMER` 唤醒、`has_animations()` 决定 arm/disarm（§11） |
| **S-92** | `InputEvent` 的指针/滚轮变体加 `modifiers`；`InputOutcome` 迁到 `event.rs`，`test_support/input.rs` 删除；`TestApp::inject` 走与平台相同的 `App::dispatch_input`（§5 / §14.9） |
| **S-93** | `platform/windows/event_loop.rs` 新增：`WM_MOUSELEAVE` → `PointerLeave`、`WM_TIMER(id=1)` → `wake`、每轮 `SetCapture`/`ReleaseCapture`/`SetCursor`/`SetTimer`/`KillTimer`、`app.sync_clock()`；`wndproc` 仍**不调用 `App`**（只写 `WndProcState` + 唤醒） |
| **F-98** | `Node` 新增**一个**字段 `interaction: Option<Box<NodeInteraction>>`（+8 B）：滚动槽 / 虚拟列表槽 / 浮层槽 / `on_click` / `on_drag` 全在同一个 `Box` 里，`Node` 从 **1128 → 1136 B**（P06 预算 1128 / 上限 1152）；手写 `Debug` 必须同步 |
| **S-94** | `interaction.rs` 是**公共模块根**（P01 §4.2）：契约类型在根文件、实现在 `interaction/{focus,scroll,virtual_list,overlay,drag,animation}.rs`；但**每个类型只有一个 `impl` 块**（`multiple_inherent_impl = "forbid"`）——交互方法写进 `app.rs` / `window.rs` / `tree.rs` 里**已存在的** impl，子模块只放 `pub(crate)` 自由函数 |
| **S-95** | 编号对齐：`node/tree.rs:319` / `:949`、`layout/hit_test.rs:444`、`node/tree.rs:1180` 的「P13」与 `paint/style.rs:101` / `:107` 的「P12」在本阶段一并改为 **P10**（纯注释）；`app.rs:122` 的 `P11/P17` 与 `render.rs` / `platform.rs` 的端口编号**不动** |
| **V-65**…**V-72** | 见 §17（`ORDER` 无清除者、浮层无碰撞规避、拖拽无抛掷惯性、焦点环不随主题、`WM_MOUSELEAVE`/捕获/滚轮/定时器抖动的实机未验证） |

---

## 附录 A · 实现者检查单（按执行顺序）

1. `paint/style.rs`：+`FocusRing` + `Style.focus_ring`（先落，paint 与 builder 都依赖它）。
2. `interaction.rs`：契约类型 + `NodeInteraction`（先落空类型，字段随子模块补齐）。
3. `node/tree.rs`：`Node.interaction` 字段、`place_subtree` 的 `content_shift` / `damage_clip`、
   `reset_layout_stats`、`scroll_slot`、`action_chain`、`set_virtual_row`；`Debug` 与新字段同步（N4 的手写 `Debug`）。
4. `event.rs`：`Action` / `KeyBinding` / `ActionSource` / `InputOutcome`；`InputEvent` 加 `modifiers`。
5. `interaction/focus.rs` → `app.rs` 的 `set_focus`/`focus_next`/`focus_prev`（消费者：`dispatch_action`）。
6. `interaction/scroll.rs` → `App::{make_scroll, set_scroll_content, scroll_to, scroll_by, scroll_state}`。
7. `interaction/virtual_list.rs` → `App::{make_virtual_list, set_virtual_items, virtual_list_state, virtual_row_index}`。
8. `interaction/overlay.rs` → `App::{create_overlay, close_overlay, place_overlay}`。
9. `interaction/drag.rs` → `Window.pointer` + `App::{drag_state, drag, cancel_drag}`。
10. `interaction/animation.rs` → `App::{animate, cancel_animation, animations_running, has_animations, tick_animations, sync_clock}`。
11. `app.rs`：`dispatch_input` / `dispatch_action` / `on_action` / `off_action` / `bind_key` / `unbind_key` / `read`。
12. `paint/pass.rs`：焦点环。
13. `layout/hit_test.rs`：`interactive: false` 不作为目标（+ 1 个既有用例核对）。
14. `platform/event.rs`：`PointerLeave` + 指针事件带修饰键；`platform/windows/window.rs`：类光标。
15. `platform/windows/event_loop.rs`：`WM_MOUSELEAVE` / `WM_TIMER` / 捕获 / 光标 / 定时器 / `sync_clock`。
16. `test_support/{app,input}.rs`：inject 全量管线 + `advance`；删 `input.rs`。
17. 注释编号对齐（§0.2 的 6 处）。
18. 单测 60 项 → 门禁（§15）→ DEVIATIONS §十五 落 15.4 实施记录。

## 附录 B · 与上游文档 / 既有代码的交叉引用

| 本文 | 上游 / 代码 |
|---|---|
| §3.2 状态归属 | `design/phase-03-node-state.md` §5.1 / §11.1；`design/phase-06-paint-runtime.md` §9.5 |
| §4 Action | 主设计 §3.6 / §7.7；`design/phase-01-foundation.md` §4.5 路径表 |
| §5 分发 | 主设计 §3.6；`design/phase-04-layout.md` §12；`platform/event.rs` / `event_loop.rs` |
| §6 焦点 | 主设计 §7.8 / §8.4；`design/phase-03-node-state.md` §10.2（`StylePatch.interactive`） |
| §7 滚动 | 主设计 §3.4 行 7 / §3.7 / §8.4；`node/tree.rs::place_subtree`；`render/damage.rs::push` |
| §8 虚拟列表 | 主设计 §7.4（a11y 只暴露可见窗口）/ §8.4；`design/phase-04-layout.md` §10 |
| §9 浮层 | 主设计 §4.4 / §8.4；`layout/hit_test.rs:444` 的既有用例 |
| §10 拖拽 | 主设计 §8.4；`design/phase-04-layout.md` §10.2 / §10.4 |
| §11 动画 | 主设计 §3.7 / §3.5（timer 唤醒）；P09 F-88 + 本阶段 probe 12/13 |
| §14 测试 | 主设计 §6.6（无 GPU harness）/ §6.9（门禁）；`design/phase-06-paint-runtime.md` §10.4 |
