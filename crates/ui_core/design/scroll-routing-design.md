# 嵌套滚动路由（Scroll Routing）设计与任务计划书

| 项 | 值 |
|---|---|
| 状态 | REVIEWED（两轮独立审查完成并已回改；待用户终审；实施前不再修改生产代码） |
| 日期 | 2026-11-26 |
| 作用域 | `crates/ui_core`（引擎：滚动模型 / 目标解析 / 序列仲裁 / 双轴）、`crates/ui_kit`（滚动条代理、双轴演示、gallery 验收）、`crates/*_ui` 调用点 |
| 输入 | 用户实机反馈（gallery 嵌套滚动优先级）、源码走查、Chrome/Qt/Flutter 源码级调研、TestApp 探针、真实窗口 OS 级滚轮复现 |
| 结论 | 采用 Chrome 式“单目标闩锁 + 新序列重仲裁”，滚轮只服务 hover 命中的目标容器；滚动条/表头等装饰节点通过显式代理归属其容器；行单位滚轮立即到位；本批一并交付统一目标解析与双轴（E22）容器能力 |

## 0. 决策记录（用户已裁决 2026-11-26）

| 编号 | 决策点 | 用户裁决 | 对计划的影响 |
|---|---|---|---|
| D1 | 路由模型 | **Chrome 式：单目标闩锁** | 一次连续滚动序列只服务序列开始时选出的一个容器；到边界后余量丢弃，序列结束后再次滚动才重新仲裁（可能轮到外层）。删除现有“同一事件内逐祖先链式消费”。 |
| D2 | 滚动条归属 | **滚它所属的容器** | 引入节点级 `scroll_proxy`；`ScrollBar` track/thumb、`Table` header、多行 `Input` clear 按钮等装饰节点显式归属其滚动容器。 |
| D3 | 行单位滚轮动画 / 滚动动画能力 | **改为立即到位；滚动动画能力整体删除（裁决 C）** | 删除“行滚轮 120 ms 动画 + 动画目标累加”，并删除 `scroll_to_animated` 与 `AnimationTarget::Scroll`；**行滚轮与程序化滚动均立即 `scroll_to`**，不再保留滚动动画 API。 |
| D4 | 实施范围 | **路由 + 统一目标解析 + 双轴容器（E22）** | 新增 `ScrollAxes` 双轴模型；滚轮/键盘/a11y 共用同一目标解析器；`ScrollSlot` 从单轴改为双轴。 |
| D5 | 附加项 | 滚动后重算 hover；gallery `--wheel-check`；gallery 外层页面加可见滚动条 | 纳入 P2/P3。**不新增**每容器链式策略 API（未选中；按 AGENTS“不做预防性抽象”暂缓）。 |
| D6 | 文档落点 | `crates/ui_core/design/scroll-routing-design.md` | 本文件；实施完成后回写偏差（附录 D）。 |

> 未选中的选项（每容器 `overscroll-behavior` 式配置、Flutter 式永不链式、Qt 式逐事件冒泡、仅最小修滚轮）不进入本批；§9 记录为后续候选。

## 1. 摘要

### 1.1 问题

`ui_core` 的滚轮路由目前是 `on_scroll` 里一段**无状态、逐事件重算**的内联逻辑：命中节点 → 沿布局父链收集所有 `ScrollSlot` → 最近者先消费、余量继续向上。它带来三类用户可见问题：

1. **滚动条陷阱**：`ui_kit` 所有滚动条（以及 `Table` 表头、多行 `Input` 的清除按钮）都是滚动视口的**兄弟节点**。滚轮落在这些节点上时，父链里没有所属的 `ScrollSlot`，于是滚到了外层页面（或什么都不滚）。真实 gallery 上已复现：在 ⑦ 演示的滚动条上滚 1 个刻度，页面整体上移 **144 逻辑 px**（180 物理 px，即 3 行 × 48 px），内层滑块不动。
2. **边界双动**：内层到达边界后，同一段连续滚动的余量立即驱动外层；加上行滚轮 120 ms 动画，视觉上两个区域同时移动。用户描述为“滚动混乱”。
3. **无序列/手势锁定**：每个事件都按当前鼠标位置重新仲裁；指针跨容器边界时目标会中途切换。

### 1.2 方案（一句话）

把“滚轮滚谁”从内联循环提升为引擎级机制：**统一目标解析器**（pointer / focus / a11y / explicit 四种来源，共享代理归属与双轴模型）+ **滚动序列会话**（Chrome 式单目标闩锁，新序列重新仲裁）+ **节点级滚动代理**（滚动条/表头等装饰节点归属其容器）+ **行滚轮立即到位**。同时把 `ScrollSlot` 从单轴升级为双轴（E22），使新模型一次到位、不再为单轴假设打补丁。

### 1.3 交付物

| 交付物 | 位置 | 验收 |
|---|---|---|
| 双轴滚动模型 | `crates/ui_core/src/interaction.rs`、`interaction/scroll.rs` | `ScrollAxes`、双轴 clamp/consume；单测 + 全部调用点编译通过 |
| 滚动路由内核 | 在 `crates/ui_core/src/interaction/scroll.rs` 增加 `scroll_candidates`/`route_wheel`/`ScrollSession`（不新建 `scroll_router.rs` 结构体层） | 目标解析、序列闩锁、代理、hover 重算；§7 用例红→绿 |
| 滚动条/装饰节点代理 | `crates/ui_kit/src/kit/{scroll_bar,list,table,input,tree}.rs`、`ui_core::App::set_scroll_proxy` | 滚轮在条/表头/清除按钮上滚所属容器 |
| 双轴演示 + 页面滚动条 + `--wheel-check` | `crates/ui_kit/examples/gallery.rs` | 三端可重复；与其他 check 统一退出码 3 |
| 文档回写 | 本文件 + `design/custom-gui-engine-design.md`（E22 状态、API 清单） | 审查通过 |

### 1.4 工作量与门禁

- 预计 5.5–7 个开发日（P0 0.5、P1 1.5、P2 2、P3 1、P4 1；含测试与三端验证）。
- 每阶段必须通过：`cargo test`（对应子集）→ `cargo +nightly clippy -- -D warnings` → `cargo +nightly fmt`；最终 `cargo test --workspace`。
- 每个缺陷修复必须有“先红后绿”用例；关键分支做变异自检（恢复旧代码必须红）。

## 2. 现状调查与根因（证据链）

### 2.1 现有模型与代码路径

**滚动模型（单轴）**

- 滚动容器 = 节点上的 `ScrollSlot { offset, content, axis, line_height }`（`crates/ui_core/src/interaction.rs:204-210`），`axis` 是 `ScrollAxis::{Vertical, Horizontal}`（`interaction.rs:153-157`）。
- 视口 = 节点 border box 内缩 border（`interaction/scroll.rs:35-40`）。
- `max_offset` / `clamp_offset` 把非本轴分量强制为 0（`interaction/scroll.rs:15-32`）；无滚动范围时恒为 0。
- 滚动容器登记在 `NodeInteraction.scroll`（`interaction.rs:593-601`），由 `App::make_scroll` 创建（`app/scroll.rs:22-49`），要求 `Clip::Bounds`。
- 虚拟列表复用同一个 `ScrollSlot`（`app/virtual_list.rs:14-49`），滚动时重排行窗口（`scroll_to` 内 `rewindow_virtual`，`app/scroll.rs:143`）。

**滚轮入口**

- 平台：Windows `WM_MOUSEWHEEL` → `ScrollUnit::Line` + `ScrollPhase::Discrete`，每刻度 `lines_per_tick` 行（`platform/windows/event_loop/native_window.rs:193-208`、`platform/windows/event_loop/pump.rs:102`、`platform/event.rs:138-157`）；macOS 走像素/连续相位（`platform/macos/view.rs:327`）；Wayland 的 wheel 为 Line、触控板为 Pixel（`platform/linux/surface.rs:2087`、`platform/linux/surface.rs:2430-2445`）。
- `dispatch_input_tracked`：先记录指针位置（`app/input.rs:128-132`），再进入 `App::on_scroll`（`app/input.rs:167-182`、`574-683`）。

**`on_scroll` 现行算法**（`app/input.rs:574-683`）

1. `hit_at(position)` 取命中节点（`584-588`）；模态作用域过滤，作用域外静默丢弃（`589-596`）。
2. Shift 交换轴（`597-601`）；`direct = Pixel || phase.is_continuous()`（`605`）。
3. 从命中节点沿**布局父链**收集带 `ScrollSlot` 的祖先，组成 `SmallVec<[…; 8]>` 的 `chain`（`609-627`）。
4. 逐项按容器自己的单轴计算 `drives`、`before`、`after`、`consumed`；`consumed` 从 `dx/dy` 扣除后继续给下一个祖先（`630-682`）。
5. 行单位走 `scroll_to_animated`（120 ms），并用动画目标做同帧累加器（`639-646`）；其余走立即 `scroll_to`（`670-674`）。

**其它滚动目标解析路径（互不一致）**

| 来源 | 解析方式 | 代码 |
|---|---|---|
| 滚轮 | hover 命中 → 父链收集 → 逐祖先消费 | `app/input.rs:584-682` |
| 键盘 | 焦点祖先上的最近 `ScrollSlot`，无链式、无回退 | `app/input.rs:818-864` |
| a11y | 自身 → 集合根提升 → 最近祖先；方向/步进另算 | `a11y/action.rs:432`、`a11y/tree.rs:804-812` |
| 滚动条拖拽/轨道点击 | 直接持有 `ScrollHandle` | `ui_kit/src/kit/scroll_bar.rs:277-293`、`428-519` |
| 程序化 | 显式 `ScrollHandle` | `app/scroll.rs:99-238` |

**状态**：`PointerState` 只有 hover / press / pending_drag / drag / capture / last_hit / modifiers / text_edit（`interaction.rs:118-129`）；没有任何滚动序列、目标锁定或“上次是哪个容器在滚”的状态。`ScrollPhase::{Begin,Update,End,Momentum,Cancel}` 只被 `is_continuous()` 用于判断“是否立即执行”（`event.rs:305-320`），没有参与目标生命周期。

**绘制耦合（不能依赖槽位号做会话）**：滚动快路径只在“无嵌套滚动槽”时成立；任一变化容器有嵌套槽位则退回整帧遍历（`window/paint.rs:255-304`，`has_nested_scroll_slot` 在 `298-304`）。滚动槽编号是绘制期产物（`paint/pass.rs:493-524`），全量重绘会清空并重排（`paint/pass.rs:108-117`），**不能作为稳定会话键**；会话状态必须持有 `NodeId`。

### 2.2 已复现缺陷（缺陷 → 证据 → 根因）

> 证据两类：**(a) TestApp 探针**（仓库外 `E:\kytrix-scratch\scrollprobe`，不修改仓库）；**(b) 真实窗口**（`E:\kytrix\target\debug\examples\gallery.exe --scroll-to=1000`，OS 级 `PostMessage(WM_MOUSEWHEEL)` + DPI-aware 截图；本机 125% DPI，客户区 1500×1000 物理像素）。

| ID | 场景 | 期望 | 实测 | 结论 |
|---|---|---|---|---|
| A | 内层**内容区**滚轮（普通嵌套 / 虚拟列表） | 只滚内层 | 探针 S1：内层 144/288/328，外层 0；探针 S4（page→section→list→虚拟列表）：列表 144/184，page 0。**实机 3 刻度会同时触发 C 的边界余量（外层 88），不能作为“只滚内层”证据**；实机内容优先结论以 S1/S4 为准 | **内层优先本身是对的**，排除“完全不区分内层” |
| B | 内层**滚动条 track** 上滚轮 | 滚内层 | 探针 S3：page 144，内层 0；真实 ⑦：页面整体上移 144 逻辑 px（180 物理 px；截图像素差测量），内层行号/滑块不动 | **缺陷**：代理归属缺失 |
| C | 内层到边界后继续滚 | 按 D1（Chrome）本次序列停住 | 真实 ⑦ 滚 3 个刻度：内层到 max 344，同一序列余量 88 逻辑 px（432−344）驱动外层（截图像素差 110 物理 px = 88 逻辑 px） | **按 D1 需改**：当前是 Qt 式链式 |
| D | 连续滚动中指针跨容器 | 目标不变 | 代码：每事件重新 `hit_at` + 重建 chain（`input.rs:584-627`） | **缺陷**：无序列锁定 |
| E | 滚动后指针下内容变化 | hover/tooltip/滚动条状态更新 | `revalidate_hover` 只在模态浮层创建（`app/overlay.rs:60`）与置顶（`:153`）调用；它只按已有 hover 节点重过滤模态作用域，`scroll_to`/关闭都不按指针重新命中 | **缺陷**（D5 纳入） |
| F | 行滚轮手感 | 立即 3 行 | 120 ms 动画（`input.rs:605`、`639-646`） | **按 D3/裁决 C 需改**：行滚轮立即；滚动动画能力删除 |
| G | 双轴容器（宽表格/大图） | 横竖都能滚 | `ScrollSlot` 单轴，非本轴恒 0（`interaction/scroll.rs:15-32`）；设计文档登记 E22 未实现（`design/custom-gui-engine-design.md:4780-4781`） | **按 D4 本批实现** |

**缺陷 B 的结构性根因**（`codebase-survey.md` §6）：

- `List`：`outer` 下同时挂 `viewport`（有 `ScrollSlot`）与 `scrollbar:track`（`ui_kit/src/kit/list.rs:570-607`）；`attach` 把 bar 绑到 viewport（`609-646`），但树里 bar 是 viewport 的**兄弟**。
- `Input`（多行）：bar 建在 `self.root`（`ui_kit/src/kit/input.rs:643-686`），viewport 是另一个子节点。
- `Table`：`header` 与包含 `List`（滚动节点）的 `body` 是兄弟子树（`ui_kit/src/kit/table.rs:484-497` vs `625-637`）→ 表头上滚轮不滚表体。
- `Input` 的 clear 按钮同样挂在 `root`（`kit/input.rs:567`）。
- 结论：**所有** `ui_kit` 滚动条子树都不在滚动槽子树内；`on_scroll` 只走父链，必然跳过所属槽（`input.rs:616-627`）。

**缺陷 C/D 的根因**：`on_scroll` 没有序列概念，链式消费是“单事件内”的；`ScrollPhase` 未参与目标生命周期；行滚轮的动画目标累加又让“边界切换”与“动画时序”耦合。`AnimationTarget::Scroll`（经 `scroll_animation_target()` 读取，`app/animation.rs:15-29`）是当前唯一**滚动目标**跨事件状态，但它只服务“同槽动画累加”，不是目标锁定；`ScrollSlot.offset` 是真实偏移，不是目标状态。

**缺陷 E 的根因**：`revalidate_hover` 的语义是“按已有 hover 节点重过滤模态作用域”（`app/input.rs:1030-1036`），不是“按指针位置重新命中”；滚动改变内容位置后没有重新命中的入口。

### 2.3 受影响组件与结构

| 组件 | 滚动槽所有者 | 滚动条挂载 | 滚轮在条/装饰节点上现状 |
|---|---|---|---|
| `List` / `Table` 表体 / `Tree` | `{key}:viewport` | `outer`（viewport 兄弟） | 滚外层页面；浮层内则无反应 |
| `Table` | 表体 `List` 的 viewport | 表体 `List` 的 `outer` | 表头上滚轮不滚表体 |
| 菜单 / 下拉 / 选择 / 右键菜单面板（`ItemPanel` → `List`） | 面板 `List` 的 viewport | 面板 `List` 的 `outer` | 面板外无滚动祖先时无反应 |
| 多行 `Input` | `field.viewport` | `field.root`（viewport 父） | 滚外层页面 |
| gallery ⑦ 自绘滚动条 | `viewport` | `outer`（viewport 兄弟） | 滚外层页面 |
| gallery 外层页面 | `gallery.viewport` | **无滚动条控件** | 页面滚动无可见反馈 |
| `settings_ui` / `updater_ui` / `candlestick` / `widget_gallery_ui` | 各自 viewport | 各自 outer/band | 与上同类 |

### 2.4 现有测试锚点（本批将有意替代/删除）

| 测试 | 现断言 | 本批处置 |
|---|---|---|
| `wheel_events_in_one_frame_accumulate`（`app/interaction_tests.rs:107-144`） | 同帧两次行滚轮 = 96 px | 保留；改为“立即累加”语义（期望值不变） |
| `wheel_falls_through_to_the_outer_container_when_the_inner_cannot_scroll`（`:1416-1484`） | 内层 max=0 ⇒ 外层 48 | 保留；新语义下由“候选选择跳过不可消费内层”实现 |
| `a_two_dimensional_wheel_delta_is_split_across_the_scroll_chain`（`:3820-3885`） | 内层横 12、外层纵 3（逐轴拆分） | **删除并替代**：单目标闩锁下横内层吃掉整段（外层 0） |
| `pixel_scroll_follows_immediately_while_line_scroll_is_animated`（`:3122-3171`） | Line 走动画、Pixel 立即 | **删除并替代**：两者都立即 |
| `programmatic_scroll_cancels_a_pending_wheel_animation`（`:3656-3704`） | 滚轮动画被 `scroll_to` 取消 | **删除**：滚动动画能力删除（裁决 C），该用例与 E31 一并删除 |
| `key_up_lands_and_arrow_keys_scroll_the_focused_container`（`:2944-3024`） | 焦点容器方向键/翻页 | 保留；补代理与双轴用例 |
| `wheel_...`/nested scroll 相关（`:719-781`、`:783-880`） | 嵌套槽位裁剪/烘焙 | 保留（绘制路径不应回归） |
| `a11y/action.rs:1603` 等 | a11y 滚动收敛到容器 | 保留；双轴/代理后补 a11y 用例 |

## 3. 外部成熟方案调研结论

> 三份源码级备忘录：`E:\kytrix-scratch\research\{chrome,qt,flutter}.md`；本节只摘录与决策相关的机制，完整引用见附录 B。

### 3.1 Chrome / Chromium

- **投递/命中**：wheel 在合成器侧由 `InputHandler::HitTestScrollNode` 命中（滚动条层被视为“scroller 或其滚动条”，`input_handler.cc:1835`）；DOM 侧 `MouseWheelEventManager::HandleWheelEvent` 按光标位置命中并**缓存 `wheel_target_` 整个手势**（`mouse_wheel_event_manager.cc:41`、`:54-79`）。
- **仲裁**：`FindNodeToLatch`（`cc/input/input_handler.cc:2341`）从命中节点沿 `ScrollTree` 上溯，选**第一个 `CanConsumeDelta` 的节点闩锁**；当 `CanPropagate` 为 false（`overscroll-behavior` 的 `contain/none` 可见语义，内部 `kChain` 虽非 auto 但仍允许传递）时仍闩锁到该节点并 `set_is_scroll_chain_cut`（`:2401-2410`）。
- **序列内不结转**：`ScrollLatchedScroller`（`:2195`）只对闩锁节点应用 delta；余量记入 `overscroll_delta_for_main_thread_ += delta - applied_delta`（`input_handler.cc:2281`），**不会在同 tick 传给祖先**。合成器 `CurrentlyScrollingNode` 在 `ScrollEnd` 前不变（`chrome.md` §3.3）；DOM 侧 `wheel_target_` 仅在带 phase 信息时缓存整个手势（`mouse_wheel_event_manager.cc:72-78`），无 phase 时每事件重新命中。
- **手势边界**：wheel/触控板手势 = `PhaseBegan/Ended`；无 phase 信息时由 `MouseWheelPhaseHandler` 定时器兜底（`mouse_wheel_phase_handler.cc`；具体超时常量未验证，见备忘录 §8）。
- **边界重仲裁**：内层到边界后，本次手势不滚外层；下一次手势的 `FindNodeToLatch` 会跳过不可消费的内层、闩锁到外层 → 表现为“停顿后继续滚外层”。
- **对角**：不是逐轴拆给不同祖先；用 45° 规则 + 向量投影（`:2295-2309`）。本批**不采纳**（见 3.5）。
- **滚动条**：合成器把滚动条层算作所属 scroller；`cc::ScrollbarController` 只处理指针（拖动/点击），wheel 走 `HandleMouseWheel` 独立路径（`scrollbar_controller.cc` 无 wheel 分支）。
- **平滑滚动**：`ShouldAnimateScroll` 由设置决定；动画路径是“同一手势内不断抬高同一动画目标”（`:2211-2234`），与本引擎现状类似，但仅作为可选设置。

### 3.2 Qt（Widgets 6.8 / Quick 6.8）

- **Widgets 命中**：`QWidgetWindow::handleWheelEvent` 只按光标 `childAt(pos)` 投递（与焦点无关），真正冒泡在 `QApplication::notify` 的 `QEvent::Wheel` 分支：逐层 `notify_helper`，某层 accept 即停，否则 `w = w->parentWidget()`；`isWindow()` / `WA_NoMousePropagation` 终止（`qapplication.cpp:2813`）。
- **限位即 ignore**：`QAbstractScrollArea::wheelEvent` 按主分量轴把一个滚动条转发同一事件（`qabstractscrollarea.cpp:1170`）；`QAbstractSlider::wheelEvent` 只在 `scrollByDelta` 返回 true 时 accept（`qabstractslider.cpp:732`），而 `scrollByDelta` 在 `prevValue == value` 时返回 false（`:721-723`）——不足一行但朝可动方向时返回 true 并吞掉事件（`:701-710`）→ **内层到限位 ignore，事件立即冒泡给父容器**（Qt 式链式）。
- **相位锁**：`QApplicationPrivate::wheel_widget` 在触控板 `ScrollBegin` 时锁定接收者，`ScrollUpdate/Momentum` 期间不因光标移动换目标；普通滚轮（`NoScrollPhase`）无锁（`qapplication.cpp:2813` 附近）。
- **滚动条**：`QScrollBar::wheelEvent` 处理自身 range（`qscrollbar.cpp:475`）→ 滚轮在滚动条上滚所属滚动区；限位后 ignore 继续冒泡。
- **Quick**：`QQuickWindow::wheelEvent` → `deliverSinglePointEventUntilAccepted` 沿 `pointerTargets`（深→浅）投递；`QQuickFlickable::wheelEvent` 到边界 `acceptEvent=false` → 父 Flickable 接手（`qquickflickable.cpp:1621`）；`QQuickScrollBar` **没有** wheel 处理（`qquickscrollbar.cpp` 无匹配）→ Quick 里滚动条上滚轮不滚内容（与 Widgets 相反）。

### 3.3 Flutter（stable `ae9b586f34a8`）

- **两阶段仲裁**：每个 `PointerSignalEvent` 重新命中；`Scrollable` 仅在“delta ≠ 0 且夹紧后目标 ≠ 当前 pixels”时向 `PointerSignalResolver` 登记（`scrollable.dart:955-984`）；`register` 只接受**第一个**（命中路径最深者先登记），派发结束后 `GestureBinding` 统一 `resolve`（`pointer_signal_resolver.dart:86-93`、`binding.dart:536-545`）。无人登记则 `respond(allowPlatformDefault: true)`。
- **嵌套**：没有 delta 结转。内层还能动（哪怕 1 px）→ 内层独占、夹紧、余量丢弃；内层恰在边界 → 不登记，最深的、能动的祖先拿到**整个** delta（`flutter.md` §2；边界行为为源码推导，官方文档未明示）。
- **对角**：单赢家，赢家只应用自己轴的分量；不存在“水平给内层、垂直给外层”的拆分（`scrollable_test.dart:1723-1785`）。
- **滚轮语义**：立即跳变（`ScrollPosition.pointerScroll` → `forcePixels`，无动画、取消 fling）；永不越界、不触发 overscroll；Windows 默认约 100 逻辑 px/格（`flutter_window.cc:470-478`）。
- **NestedScrollView**：私有 `_NestedScrollCoordinator` 把 inner/outer position 当成一个会话分配 delta（`nested_scroll_view.dart:956-1032`）——这是“需要联动时显式协调”的范式，不是默认行为。
- **滚动条**：`RawScrollbar` 是 `Scrollable` 的祖先（overlay），wheel 走同一 resolver，通常仍是更深的 `Scrollable` 先登记；滚动条自己的 handler 是兜底，两者都调用同一个 `position.pointerScroll`（`scrollbar.dart:2362-2384`、`:710-729`）。

### 3.4 横向对比

| 维度 | Chrome | Qt Widgets | Qt Quick | Flutter |
|---|---|---|---|---|
| 目标来源 | 光标命中（滚动条算所属 scroller） | 光标命中（childAt） | 命中链（含 parentItem） | 命中路径（深→浅） |
| 谁决定 | `FindNodeToLatch` 选第一个可消费节点 | 逐层 accept/ignore | 逐层 accept/ignore | 第一个“愿处理”的 Scrollable |
| 序列内是否换目标 | 否（闩锁） | 普通滚轮是；触控板 phase 锁 | **否（无跨事件目标锁；每个 `QWheelEvent` 重新 `pointerTargets`；`lastWheelEventAccepted` 仅抑制兼容事件）** | 每个 signal 独立命中（无显式锁） |
| 内层到边界 | 本次手势停；新手势重新仲裁 → 外层 | 同一事件立即冒泡外层 | 同一事件冒泡父 Flickable（部分越界时子滚剩余量 + 仍 ignore，父按整包再处理） | 内层不登记 → 祖先拿整包 delta（源码推导，未官方文档确认） |
| 序列内余量结转 | 否 | 是（逐层） | 是（逐层，按整包） | 否 |
| 对角拆分 | 45° 规则/投影；不拆给不同祖先 | 按主分量轴只转一个 scrollbar | 分轴 `xflick/yflick`，不匹配轴跳过 | 单赢家，只吃自己轴 |
| 滚动条上滚轮 | 合成器：所属 scroller；DOM/main-thread：frame scrollbar → `documentElement` | 滚动条自身（=所属滚动区） | 不滚内容（冒泡） | 通常由更深的 Scrollable 获胜（源码顺序推导，未运行时验证） |
| 行滚轮动画 | 可配置平滑滚动 | 立即 | 立即/像素渐进 | 立即 |

### 3.5 采纳与不采纳

**采纳**

1. **命中决定目标、焦点只服务键盘**（三者一致）。
2. **序列闩锁**：一次连续滚动只在开始时仲裁一次目标；序列内不因指针移动换目标（Chrome；Qt 触控板同）。
3. **可消费才认领**：目标必须能真正改变 offset；否则沿候选链继续向上（Chrome `CanConsumeDelta` / Flutter“不登记”）。
4. **滚动条/装饰节点归属其容器**（Chrome 合成器、Qt Widgets、Flutter 实际效果一致；Qt Quick 是反面例子）。
5. **整包消费、边界不结转**（Chrome 序列内 + Flutter）：目标把 delta 夹紧后吃掉，余量丢弃；到边界后本次序列停住，新序列再仲裁。
6. **立即跳变**：Qt Widgets/Flutter 立即；Chrome 受 `enable_smooth_scroll` 设置、Qt Quick 有 `wheelDeceleration` 渐进；本引擎按 D3 选择立即。
7. **双轴容器**按 `ScrollAxes` 建模；单轴容器只消费自身轴，非本轴分量丢弃（Chrome/Flutter 都不把对角分量拆给不同祖先）。

**不采纳**

1. **Qt 式逐事件冒泡/边界立即结转**：D1 已裁决 Chrome 式；保留为后续可选。
2. **Flutter 式永不链式**：桌面嵌套页面需要“到边界后继续滚外层”，Chrome 式用“新序列”满足；Flutter 需移动鼠标，桌面不友好。
3. **Chrome 45° 投影**：本批简化为“候选链中第一个可消费容器 + 目标按自身启用轴整包消费”；差异登记 §9-O1。**（已被 addendum A1 覆盖：45° 规则与向量投影精确复刻，见 `scroll-routing-addendum.md` §2 与本文附录 E）**
4. **`overscroll-behavior` 式每容器策略 API**：D5 未选；等真实需求。
5. **`NestedScrollView` 式 inner/outer 协调器**：本批不做联动滚动（如折叠头部）；等真实需求。

## 4. 目标架构

### 4.1 术语与不变量

| 术语 | 定义 |
|---|---|
| 滚动容器 | 拥有 `ScrollSlot` 的节点（可双轴） |
| 滚动代理 | 节点上的 `scroll_proxy: Option<NodeId>`，表示“命中我等同于命中目标容器” |
| 候选链 | 从命中/焦点节点出发，按“代理优先 + 祖先”规则得到的有序容器列表（近 → 远） |
| 滚动序列 | 一次连续滚轮/触控板手势；离散滚轮以“事件时间戳间隔 ≤ `SCROLL_SEQUENCE_IDLE`”判定 |
| 闩锁目标 | 序列开始时选出的唯一滚动容器；序列内不变 |

**不变量**

- **I1 命中决定目标**：滚轮目标只由指针位置的命中/代理决定，焦点不参与；键盘目标只由焦点决定。
- **I2 单目标**：一个滚动序列内最多一个滚动容器接收滚轮；不存在同一事件内多容器消费。
- **I3 可消费才认领**：目标必须能改变其 offset；不能改变时沿候选链上溯。
- **I4 整包消费**：目标消费整段 delta（夹紧后），余量丢弃；不向祖先结转、不按轴拆给其他容器。
- **I5 序列稳定**：序列内 D1 锁定的目标不变（节点被卸载/窗口失焦/浮层变化除外）。
- **I6 代理显式**：装饰节点（滚动条、表头、清除按钮）必须显式声明 `scroll_proxy`，不得依赖“恰好是祖先”。
- **I7 无隐藏副作用**：没有实际滚动时不出帧、不唤醒、不标脏；滚动只走既有 damage/槽位路径。
- **I8 双轴可扩展**：消费按 `(容器, 轴)` 进行；单轴容器只消费自身轴，双轴容器消费两轴。
- **I9 失效安全**：链/会话中的节点可随时被卸载；下一事件必须重新解析，不 panic、不访问悬空节点。

### 4.2 数据模型

**双轴（D4）**

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollAxes {
    Vertical,
    Horizontal,
    Both,
}

impl ScrollAxes {
    pub const fn vertical(self) -> bool;
    pub const fn horizontal(self) -> bool;
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollConfig {
    pub axes: ScrollAxes,
    pub line_height: LogicalPx,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollState {
    pub offset: LogicalPoint,
    pub content: LogicalSize,
    pub viewport: LogicalSize,
    pub axes: ScrollAxes,
}

pub(crate) struct ScrollSlot {
    pub offset: LogicalPoint,
    pub content: LogicalSize,
    pub axes: ScrollAxes,
    pub line_height: LogicalPx,
}
```

- `ScrollAxis`（单轴枚举）**删除**；`ScrollbarAxis`（kit 的 UI 概念）保留，按 `ScrollAxes` 的某一轴构图。
- `max_offset` / `clamp_offset` 只对启用轴生效；未启用轴恒 0（`interaction/scroll.rs:15-32` 重写）。
- 默认 `ScrollConfig { axes: ScrollAxes::Vertical, line_height: px(48.0) }`。
- 代理不放在 `ScrollSlot` 里：代理节点自身不必是滚动容器，`scroll_proxy` 存在 `NodeInteraction`（见 §4.7）。

**会话（新增）**

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionKind {
    Discrete,
    Continuous,
}

pub(crate) struct ScrollSession {
    pub target: NodeId,
    pub kind: SessionKind,
    pub last_event_at: Instant,
    pub ended_at: Option<Instant>,
}
```

- 存放在每窗口的 `PointerState.scroll_session: Option<ScrollSession>`（`interaction.rs:118-129` 已按窗口持有指针状态）；窗口关闭自然清理，多窗口互不干扰。**不引入额外的 `ScrollRouter` 结构体/枚举层**——序列逻辑是 `interaction/scroll.rs` 上的自由函数 + `ScrollSession` 方法。
- `SCROLL_SEQUENCE_IDLE = Duration::from_millis(200)`（模块常量；Chrome 的相位超时未在源码中核实，取 100–200 ms 的建议值，实施期用真机调参并记录）。
- 时间基准用 `InputEvent::Scroll.timestamp`（平台消息时间），不用 `Instant::now()`；测试可构造带时间戳的事件，确定性验证序列边界。

### 4.3 目标解析

**候选链**（替代 `on_scroll` 的父链收集）

```text
scroll_candidates(from):
    out/visited/hops
    cursor = from
    while cursor:
        hops += 1; if hops > MAX_CHAIN_HOPS or visited.contains(cursor): break
        visited.push(cursor)
        if let Some(proxy) = scroll_proxy(cursor):
            # 连续代理链 A→B→C：沿代理一直跟随，中间节点不产出；失效/环/hop 超限即停
            chain = follow_proxy_chain(proxy)       # 逐跳校验 scroll_slot、visited、hops
            if chain is valid:
                push_unique(out, chain.final_target)
                cursor = parent(chain.final_target)  # 目标祖先仍在链上
                continue
            # 失效代理：按“无代理”继续
        if scroll_slot(cursor).is_some(): push_unique(out, cursor)
        cursor = parent(cursor)
```

规则：

1. **代理优先于自身**：节点有 `scroll_proxy` 时只产出（代理链的）目标容器，不产出自身，即使自身有 `ScrollSlot`。
2. **连续代理链**：`A→B→C` 时继续跟随 `B→C`，**中间节点不产出**（含 B 的自身槽）；只产出最终目标 C，并从 `parent(C)` 继续，保证目标祖先仍在链上。代理链在“无代理 / 目标无 `ScrollSlot`（失效）/ 命中 `visited`（环）/ `hops > MAX_CHAIN_HOPS`”时停止；任一失败按失效代理处理（退化为“无代理”：产出自身槽，从 `parent(node)` 继续）。
3. **代理必须仍有效**：代理链每一跳的节点都必须存在且拥有 `ScrollSlot`；失效（目标被卸载）时按“无代理”继续 `parent(node)`，不产出死节点。
4. **环防护**：`visited` 记录走过的节点（含代理链中间节点）；`MAX_CHAIN_HOPS = 32` 是纯安全阀（代理跳也计数），与 `SmallVec` 的 inline 容量解耦；`push_unique` 返回是否新增，重复即终止。**不设 8 层硬截断**（旧实现的 `SmallVec<[…; 8]>` 只是 inline 容量，超出会 spill，不是上限）。
5. **设置时拒绝后代目标**：`set_scroll_proxy` 拒绝 `target` 是 `node` 的后代（`tree.is_ancestor(node, target)`），从源头消除最常见的 A 祖先→B 后代环；`visited` 兜底任意 A→B→C→A 环。
6. **模态作用域**：仍在事件入口过滤（保持 `on_scroll` 现有行为与 `input.rs:589-596`），不进入候选链逻辑。
7. **可见/禁用**：命中路径已排除不可见/禁用节点；但代理目标可能跨子树，`select_target` 额外要求候选 **`tree.is_effectively_visible(target)`（自身与全部祖先都可见，含隐藏祖先）&& `tree.is_effectively_interactive(target)`**。不满足的候选**跳过并继续向上**（不终止链；补 K10 用例）。

**来源映射**

| 来源 | 起点 | 选择规则 |
|---|---|---|
| 滚轮 | 命中节点（`hit_at(position)`） | 候选链 → 第一个“能消费”的容器 → 闩锁 |
| 键盘 | 焦点节点 | 候选链 → 第一个“对应轴可移动”的容器；不链式、不到边界回退（保持现状语义） |
| a11y | 目标节点 | 候选链 → 第一个容器；保留 `a11y_scroll_target` 的集合根提升钩子 |
| 显式 | `ScrollHandle` | 直接使用（滚动条拖拽、程序化） |

**“能消费”判定**（`can_consume`）

```text
输入：slot, delta（已按 Shift 换轴、按行/像素换算）, unit
对 slot.axes 启用的每个轴：
    drives = delta 在该轴的分量（Line 单位先乘 line_height）
    若 drives != 0 且 clamp(offset - drives) != offset 且该轴有滚动范围
        ⇒ 可消费
返回：任一启用轴可消费
```

- 行单位：`drives_px = delta_lines * line_height`（双轴共用同一个 `line_height`；水平行步长见 §9-O2）。
- 方向由 delta 符号决定；同一容器向一个方向可滚、另一个方向到边界时，按本次 delta 判定。
- 动画不再参与 `can_consume`（行滚轮已立即到位，无“目标偏移”需要读取）。

### 4.4 序列与闩锁

**状态机**

```text
Idle ──wheel(可消费)──▶ Active(target, kind, last_at)
Active ──wheel(间隔 ≤ IDLE)──▶ Active(同 target；只更新 last_at)
Active ──wheel(间隔 > IDLE)──▶ 重新仲裁（可能换 target）→ Active
Active ──End(Discrete)/Cancel──▶ Idle
Active ──End(Continuous)──▶ Grace(target, ended_at=now)
Grace ──Momentum/Update 且 now-last_at ≤ IDLE──▶ Grace(同 target)
Grace ──now-last_at > IDLE / Begin / Cancel──▶ Idle（Begin 重新仲裁）
Active ──目标节点失效──▶ Discrete/Update/Momentum：当前事件重新仲裁并消费；End：只清会话（Idle）
Active ──窗口失焦/模态浮层开闭/目标卸载──▶ Idle
```

**过期规则（对 `ended_at` 会话同样适用）**：任何会话若 `now - last_event_at > SCROLL_SEQUENCE_IDLE` 视为过期；下一事件先清会话再按新序列仲裁。`End(Continuous)` 只进入 Grace，不得无限期沿用旧目标。

**事件映射**

| 事件 | 行为 |
|---|---|
| `Phase::Discrete`（Windows 滚轮） | 无会话或 `now - last_event_at > IDLE` → 新序列（重新仲裁）；否则沿用目标。处理后 `last_event_at = now`、`ended_at = None` |
| `Phase::Begin` | 强制新序列：**先清旧会话**；非零 delta → 重新仲裁并建立新会话；**零 delta `Begin` 清会话但不建立新会话**（后续 `Update` 再重新仲裁），`ended_at = None` |
| `Phase::Update` | 有会话且同窗口 → 沿用；无会话 → 新序列 |
| `Phase::Momentum` | 有会话且 `now - last_event_at <= IDLE` → 沿用（即使 `ended_at` 已置位）；超过 IDLE → 清会话并按新序列仲裁 |
| `Phase::End` | 有会话时：先按当前目标处理本次事件（若有 delta），`Discrete` 会话直接清，`Continuous` 会话只置 `ended_at = Some(now)` 并把 `last_event_at = now`（进入 Grace，等待可能的 Momentum）。**无会话时不建立新会话、不滚动**（仅按收盘语义结束） |
| `Phase::Cancel` | 立即清会话，不处理 delta |
| 指针移动 | 不改变会话目标；hover 仍按普通指针路径更新 |
| 目标节点失效 | **`Discrete`/`Update`/`Momentum`：当前事件重新仲裁并处理本次 delta**（不丢事件）；**`End`：只清会话、不消费**；避免访问悬空节点 |

> **无命中（`hit=None`）**：不选目标、不滚动闩锁目标；仍执行相位状态机（`Cancel`/`End(Discrete)` 清会话、`End(Continuous)` 只收盘；`Begin`/`Update`/`Momentum` 不滚动）。详见 §4.5/§4.10。

> macOS 的顺序是 `phase Ended` 之后跟随 `momentumPhase Began/Changed/Ended`（`platform/macos/view.rs:980-999`）。若 `End` 立即清会话，惯性会被当成新序列重新命中，可能在惯性中换目标——因此 `End` 只“收盘”不清会话，`Momentum` 在 `IDLE` 宽限内继续沿用；`Begin`/`Cancel`/失焦/模态开闭才强制清。补 E27 `continuous_end_then_momentum_keeps_target`。

**为什么用时间戳而非“滚轮相位”**：Windows 只发 `Discrete`，没有 Begin/End；macOS/Wayland 触控板有 phase。时间戳方案对两者统一，且可测试（构造不同 timestamp 的事件）。

### 4.5 消费算法

> **O1 已被 addendum A1 覆盖**：本节“整包消费”的偏移应用不变，但“按自身启用轴整包消费”的记账被替换为 Chrome 的 `applied_delta` + 45° 规则 + 向量投影 + `ConsumeDelta` 记账；精确算法、边界与源码行号见 `scroll-routing-addendum.md` §2（`apply_to_target` 增量见其 §2.7）。Line 单位换算按“目标选择时逐候选、消费时用目标”的 `slot.line_height` 进行，**不存在全局换算**（addendum §2.4/§2.5）。**符号契约**：本引擎 `InputEvent.delta` 与 offset 增加方向相反，路由内部先归一化 `requested = -raw × line_height`、`goal = offset + requested`、`applied = new − offset`（只在路由内部，不修改事件语义；addendum §2.4）。本节其余条款（单目标、余量丢弃、一次 `scroll_to` 写两轴、无动画、hover 刷新）继续有效。

```text
route_wheel(window, position, delta, unit, phase, modifiers):
    hit = hit_at(position) filtered by modal scope
    if hit is None:
        # 无命中：只做相位清理，绝不滚动闩锁目标
        按相位清理会话（Cancel/End(Discrete) 清会话；End(Continuous) 只收盘；
                        Begin 清旧会话；Update/Momentum 保持）
        return
    session = session_for(window, phase, event.timestamp)
    target = match session:
        Some(s) if s.target alive => s.target
        _ => select_target(candidates(hit.node), delta, unit)   // 无候选 ⇒ return
    if target is None: return
    apply_to_target(target, delta, unit)
    if session was newly created: store(session{target, ...})
    if any offset changed:
        refresh_hover_under_pointer(window)
        request_frame (由 scroll_to 内部完成)

apply_to_target(target, raw, unit):
    slot = tree.scroll_slot(target)
    requested = request_px(slot, raw, unit)          // 归一化：offset 增加方向；Line 用目标 line_height
    application = apply_scroll(slot, viewport(target), requested)   // addendum §2.4（applied/consumed/moved）
    if application.moved:
        goal = slot.offset + application.applied     // 一次调用同时写两轴：damage、a11y、rewindow_virtual 各一次
        scroll_to(handle, goal)
    // 余量丢弃：不再循环到祖先
```

- **整包消费**：`apply_to_target` 只作用于闩锁目标；不把剩余量给候选链后续节点。
- **无命中/无会话 End**：`hit=None` 时只做相位清理，**不选目标、不滚动闩锁目标**；无会话的非零 `End` 也**不建立新会话、不滚动**（§4.4/§4.10）。
- **双轴**：同一目标内两轴合并为**一次** `scroll_to(goal)`（`goal` 从原 `slot.offset` 构造、只改需要动的轴；一次 damage / 一次 `rewindow_virtual`）。
- **无动画（能力已删除，裁决 C）**：删除 `scroll_animation_target`、`scroll_to_animated`、`AnimationTarget::Scroll` 与 `on_scroll` 动画分支；**行滚轮与程序化滚动统一立即 `scroll_to`**，不再有“动画目标”状态。
- **无滚动时**：不 `request_frame`、不刷新 hover（I7）。
- **滚动后 hover**：调用新 `refresh_hover_under_pointer(window)`：读取 `Window::pointer_position()` → `hit_at` → `update_hover`；无指针位置或指针被捕获时跳过（避免打断拖拽）。只做一次命中测试，成本与指针移动同阶。
- **`direct` 标志删除**：滚轮不再区分 Pixel/Line 的执行方式；单位只影响步长换算。

### 4.6 双轴容器（E22）规格

**引擎**

- `ScrollSlot { offset, content, axes, line_height }`；`ScrollConfig { axes, line_height }`；`ScrollState { axes }`。
- `max_offset`：仅启用轴计算 `content - viewport`，未启用轴 0。
- `clamp_offset`：启用轴夹紧，未启用轴强制 0。
- `make_scroll`：接受 `axes`；`Clip::Bounds` 与 `line_height` 校验不变。
- `set_scroll_content`：写入 `LogicalSize`（双轴共用）。
- `scroll_to` / `scroll_by`：目标点两轴分别夹紧。
- 虚拟列表：仅纵向；`make_virtual_list` 断言/设置 `axes = Vertical`（不支持横向虚拟列表；文档登记）。
- `ScrollUnit::Line` 的行步长：两轴都使用 `line_height`；`Shift+wheel` 先把 delta 换轴，再按目标轴换算。
- `ScrollPhase` 语义不变（仅用于序列生命周期）。

**kit / 应用**

- `ScrollBar` 保持“一个实例一个轴”；双轴容器使用两个 `ScrollBar`（垂直 + 水平），各自 `attach(..., node)`，两者都通过代理归属同一容器。
- `ScrollbarOptions.inset` 用于给双轴演示留出角落（水平条 `right` 内缩 = 垂直条厚度）；如实现中发现 `inset` 不足以表达，再评估是否加最小 API（§9-O3）。
- 现有组件：`List`/`Table`/`Tree`/菜单面板/多行 `Input` 保持纵向；`Input` 单行保持横向。
- 双轴演示：gallery ⑦ 升级为“双向可滚 + 两条滚动条”（内容尺寸大于视口，两个方向都可滚），作为引擎双轴能力的真实窗口证据。
- 组件级横向滚动（如宽表列）：**原计划不在本批，已被 addendum A2 覆盖**（Table 横向滚动纳入本批且全局默认生效；内容宽 = Σ Fixed 宽 + Σ Flex 最小宽，`Column::flex` 为三参数显式最小宽；表头为 Table 内部 sticky、水平条/垂直条用局部 rail 留 4 px 尾隙；架构/API/迁移/测试见 `scroll-routing-addendum.md` §1.2/§3）。原 §5.2/§5.5 的 Table 迁移条目由 addendum 扩充，不改变既有纵向代理条款。

**a11y**

- `A11yScroll`（`a11y.rs:137-144`）目前只有 `vertical: bool`，无法表达 `Both`；改为 `A11yScroll { x, y, x_max, y_max, orientation: Option<A11yOrientation> }`：
  - `scroll_state` 从 `ScrollAxes` 计算：Vertical → `Some(A11yOrientation::Vertical)`；Horizontal → `Some(A11yOrientation::Horizontal)`；Both → `None`。
  - `adapter.rs:660-668` 改为：显式 `record.orientation` 优先，否则仅当 `scroll.orientation` 为 `Some` 时设置；Both 不自动设置。
- `a11y/tree.rs:515` 的 `vertical` 推导删除，改由上述 `orientation` 表达。
- a11y 滚动动作的步长：`a11y/action.rs:455-458` 目前**所有动作**都用 `viewport.size.height`（横向动作也用 height，是既有缺陷）；本批修正为：`ScrollUp/ScrollDown/PageUp/PageDown` → `viewport.height`，`ScrollLeft/ScrollRight` → `viewport.width`，`ScrollByItem` → `line_height`。补 E32 断言。
- a11y 目标解析按动作轴过滤：`a11y_scroll_target(tree, node, axis)` 返回候选链中第一个 `axes` 包含该轴的容器；`ScrollDown` 在“横向内层 + 纵向页面”中落到外层（与滚轮 E13 一致）。补 E32b。

### 4.7 滚动代理（D2）

- `NodeInteraction` 增加 `scroll_proxy: Option<NodeId>`（`interaction.rs:593-601`）。
- 公共 API：

```rust
impl App {
    pub fn set_scroll_proxy(
        &mut self,
        window: WindowId,
        node: NodeId,
        target: Option<NodeId>,
    ) -> Result<(), EngineError>;
}
```

  - 校验：`window` 存在（`UnknownWindow`）、`node` 存在（`StaleNode`）、`target` 存在且拥有 `ScrollSlot`（`NotScrollable { node: target }`）、`target != node` 且不是 `node` 的后代（`InvalidScrollProxy { node, target }`）；`None` 清除。
  - 错误矩阵：`UnknownWindow`（窗口不存在）/ `StaleNode`（节点不存在）/ `NotScrollable { node: target }`（目标存在但无 `ScrollSlot`）/ `InvalidScrollProxy { node, target }`（自指或 `target` 是 `node` 的后代）。不引入字符串错误。
- **kit 接线**：
  - `ScrollBar::attach(app, window, scroll_node)`：对 `track` 调 `set_scroll_proxy(.., Some(scroll_node))`；`attach` 改为返回 `Result<(), EngineError>`（当前返回 `()`，`scroll_bar.rs:277-293`），由调用方处理错误；`track` 必须已有 `ScrollHandle`（`attach` 已读取），代理目标校验失败则返回错误而不是静默。`thumb` 是 `track` 的子节点，候选链会先经过 thumb → track（代理）→ 目标，无需单独设置。
  - `ScrollBar::release`：清除 track 的代理；节点可能已卸载，只忽略 `StaleNode`（`release` 返回 `usize` 的现有约定不变，内部记录清理计数）。
  - `ScrollbarAxis::point`（`scroll_bar.rs:54-59`）改为 `point(current: LogicalPoint, offset: LogicalPx) -> LogicalPoint`：只覆盖本轴、保留另一轴。`on_track_click`（`:460-462`）与 `on_thumb_drag`（`:510-512`）先读 `scroll_state(&handle)` 的当前 offset 再调用；否则双轴容器拖垂直条会把 `x` 清零。
  - `Table`：`TableInner::Geometry` 增加 `header: Option<NodeId>`（当前只存 `outer/body/header_row`，`table.rs:240-246`），`build` 时记录 header 节点；`attach` 在 `self.inner.list.attach(...)` 之后取 `list.viewport_node()` 设置 header 代理。`header` 与 `body` 是兄弟子树（`table.rs:484-497` vs `625-637`）。
  - 多行 `Input`：clear 按钮在 `set_clearable(true)` 时才创建（`input.rs:537-573`），不能在 `install_scroll`（`:643-686`）接线。改为：`set_clearable(true)` 成功后、以及 `sync_clear_button` 首次拿到 `button.node()` 时调用 `set_scroll_proxy(.., Some(field.viewport))`；`set_clearable(false)` 移除前设 `None`；`install_scroll` 对 disabled 输入直接跳过（`:463-465`），此时目标无 `ScrollSlot` 则跳过代理。`release` 清理。
- **环/悬空**：`set_scroll_proxy` 拒绝 `target` 为 `node` 后代与自指；候选链 `visited` 防任意环；目标卸载后候选链按“失效代理”跳过并重新落到祖先（I9）。
- **可见性/禁用**：`select_target` 只接受 **`tree.is_effectively_visible(target)`（自身与全部祖先可见，含隐藏祖先）&& `tree.is_effectively_interactive(target)`** 的候选；不满足的候选跳过并继续向上（不终止链）。
- **语义**：代理节点只影响“滚轮/滚轮类滚动目标解析”，不改变点击、拖拽、hover、a11y 角色。

### 4.8 统一目标解析

| 来源 | 现有实现 | 目标实现 |
|---|---|---|
| 滚轮 | `on_scroll` 内联父链 | `interaction/scroll.rs` 的 `scroll_candidates` + `ScrollSession` + 序列闩锁 |
| 键盘 | `default_key_scroll` 的 `scroll_ancestor` | 同一 `scroll_candidates(focused)`，取第一个对应轴可移动的容器；步进规则不变（方向键 1 行、Page 1 页、Home/End 两端） |
| a11y | `a11y_scroll_target`（含集合根提升） | 保留集合根提升钩子；底层“找容器”改为 `scroll_candidates` 的第一个 |
| 滚动条拖拽 | 显式 `ScrollHandle` | 不变（显式来源，不经仲裁） |

**键盘语义保持不变**：焦点容器到边界不链式、不回退；`bind_key` 仍优先于默认滚动（`input.rs:780-815`）。仅把“找容器”换成代理感知的共享解析器。

### 4.9 与滚动动画（已删除）、虚拟列表、绘制、hover 的关系

- **动画（能力已删除，裁决 C）**：滚轮与**程序化滚动都立即 `scroll_to`**；`App::scroll_to_animated`、`AnimationTarget::Scroll`、`scroll_animation_target`（`app/animation.rs:15-29`）**全部删除**；`scroll_to` 是唯一写偏移入口（不再有“取消同槽动画”语义）。
- **虚拟列表**：`scroll_to` 已调用 `rewindow_virtual`（`app/scroll.rs:143`）；滚轮立即路径无需新增钩子。嵌套虚拟列表的槽位裁剪/烘焙仍走既有 `paint.rs` 逻辑（§2.1），本批不改渲染。
- **绘制/damage**：只复用 `scroll_to` 的 damage/`a11y_note_scroll` 路径；`refresh_hover_under_pointer` 只更新 hover 绑定（`update_hover` 已有 hover 链 diff + `mark_bind/mark_paint`），不触发样式全量重算。
- **hover**：滚轮导致的实际滚动后刷新一次；程序化滚动是否刷新不在本批（避免 `ensure_visible` 等高频路径额外命中测试；§9-O4）。
- **光标/滚动条淡入**：`ScrollBar::sync` 用 `app.hovered()` 判 `node_in_subtree(track, hovered)`；hover 刷新后自然得到正确状态。
- **模块可见性**：`app::input` **保持私有**；仅以窄导出 `pub(crate) use refresh_hover_under_pointer`（或同义窄入口）供 `interaction/scroll.rs` 复用，不把整个 `app::input` 提升为 `pub(crate)`。

### 4.10 边界情况

| 情况 | 处理 |
|---|---|
| 模态浮层（dialog/dropdown/menu） | 保留入口的 modal scope 过滤：作用域外滚轮直接丢弃（现状）；作用域内正常路由。浮层开/关清会话 |
| 非模态浮层（tooltip 等） | 命中在浮层节点上时按其候选链滚动；浮层通常无滚动槽 → 落到页面（可接受） |
| 目标节点在序列中被卸载 | **`Discrete`/`Update`/`Momentum` 检测到 target 失效 → 当前事件重新仲裁并处理本次 delta**（不丢事件）；**`End` 遇失效 target 只清会话、不消费**；避免访问悬空节点 |
| 窗口失焦 | `on_window_focus_lost` 清会话（`app/input.rs:902-914`） |
| 指针离开窗口 | 会话保留到 IDLE 自然过期；滚轮不再到达 |
| 多窗口 | 会话按 `WindowId` 隔离 |
| 禁用/不可见节点 | 命中测试已排除；**代理/闩锁目标用 `is_effectively_visible`（自身与全部祖先可见，含隐藏祖先）+ `is_effectively_interactive` 过滤**；不满足的候选跳过并回退到链上的下一个 |
| 无命中（`hit=None`） | 只做相位清理：`Cancel`/`End(Discrete)` 清会话、`End(Continuous)` 只收盘；**不选目标、不滚动闩锁目标** |
| 零 delta `Begin` | 清旧会话、**不建立新会话**（后续 `Update` 再重新仲裁） |
| 无会话 + 非零 `End` | **不建立新会话、不滚动**（仅按收盘语义结束） |
| 深层嵌套（>8） | **不设 8 层截断**（旧 `SmallVec` 容量只是 inline 优化）；`visited` + `MAX_CHAIN_HOPS=32` 纯安全阀；E22 用 12 层验证第 9 层可滚 |
| 双轴 + Shift | 先换轴再选目标；换轴后只按新 delta 判定 |
| 滚动条在双轴容器 | 两个 bar 各自代理同一节点；滚轮在任一 bar 上都滚目标容器（两轴都消费） |

## 5. API 规格

### 5.1 `ui_core` 新增

| 项 | 形状 | 说明 |
|---|---|---|
| `ScrollAxes` | `enum { Vertical, Horizontal, Both }` + `vertical()/horizontal()` | 双轴能力；替代 `ScrollAxis` |
| `App::set_scroll_proxy` | `fn(&mut self, window, node, target: Option<NodeId>) -> Result<(), EngineError>` | 装饰节点 → 滚动容器；`None` 清除；错误见 §4.7 矩阵（含 `InvalidScrollProxy`） |
| `ScrollSession` | `pub(crate) { target: NodeId, kind: SessionKind, last_event_at: Instant, ended_at: Option<Instant> }` | 序列闩锁状态，挂在 `PointerState.scroll_session`；`Discrete` 的 End 清会话，`Continuous` 的 End 只置 `ended_at`；不暴露公共 API |
| `EngineError::InvalidScrollProxy` | `{ node: NodeId, target: NodeId }` | 自指或 `target` 是 `node` 的后代时使用；目标非滚动容器仍用 `NotScrollable` |
| `NodeInteraction.scroll_proxy` | `pub(crate) Option<NodeId>` | 代理存储；`NodeTree` 提供 `scroll_proxy(node)` 读、`set_scroll_proxy(node, Option<NodeId>)` 写 |
| `SCROLL_SEQUENCE_IDLE` | `pub(crate) const Duration = 200 ms` | 离散滚轮序列边界；实施期真机调参，改动只影响常量与测试 |

### 5.2 `ui_core` 修改

| 项 | 修改前 | 修改后 |
|---|---|---|
| `ScrollConfig` | `{ axis: ScrollAxis, line_height }` | `{ axes: ScrollAxes, line_height }`；`Default = Vertical` |
| `ScrollState` | `{ offset, content, viewport, axis }` | `{ offset, content, viewport, axes }` |
| `ScrollSlot` | `{ offset, content, axis, line_height }` | `{ offset, content, axes, line_height }`（`pub(crate)`） |
| `max_offset` / `clamp_offset` | 单轴；非本轴恒 0 | 双轴；未启用轴恒 0；启用轴各自夹紧 |
| `scroll::scroll_ancestor_on` | 按单轴过滤 | **已删除（P1）**；由 `scroll::scroll_ancestors` + 轴过滤 / `scroll_candidates` 替代 |
| `app/animation::scroll_animation_target` | 行滚轮动画累加 | 删除（无调用方） |
| `on_scroll` | 内联父链 + 逐祖先消费 | 委托 `interaction/scroll.rs` 的 `route_wheel`；保留入口的 modal scope 过滤与 SHIFT 换轴 |
| `default_key_scroll` | `scroll_ancestor` 单轴 | 共享 `scroll_candidates`；步进语义不变 |
| `a11y::A11yScroll` | `{ x, y, x_max, y_max, vertical: bool }` | `{ x, y, x_max, y_max, orientation: Option<A11yOrientation> }`；Both → `None`；`adapter.rs` 仅在 `Some` 时设置 |
| `a11y_scroll_target` | 自身→集合根→最近祖先 | 保留集合根提升；容器查找复用 `scroll_candidates`，并按动作轴过滤 |
| `a11y/action.rs` 步长 | 所有动作 `viewport.height` | `ScrollUp/Down/Page*` 用 height；`ScrollLeft/Right` 用 width；`ScrollByItem` 用 line_height |
| `ScrollBar` | `attach` 只记 `ScrollHandle`；`ScrollbarAxis::point` 清零另一轴 | `attach -> Result` 并设 track 代理；`release` 清理；`point(current, offset)` 只改本轴；`Table`/`Input` 见 §4.7 |
| `Table` | `Geometry` 只存 outer/body/header_row | 增加 `header: Option<NodeId>`；attach 后设 header 代理 |
| `App::scroll_to_animated` | 计划保留公共/`pub(crate)` 包装 | **删除（裁决 C：滚动动画能力移除）**；程序化滚动统一 `scroll_to`（立即），无替代动画 API |
| `app::input` | P2 曾提升模块可见性 | **模块保持私有**；仅窄导出 `refresh_hover_under_pointer`（`pub(crate)`）供 `interaction/scroll.rs` 调用 |

### 5.3 删除（不保留兼容层）

- `interaction::ScrollAxis`（单轴枚举）与所有 `axis` 字段/匹配。
- `on_scroll` 内联 `chain` 构建与逐祖先消费（`app/input.rs:609-682`）。
- `direct`（`unit == Pixel || phase.is_continuous()`）分支与行滚轮动画分支；连带删除 `ScrollPhase::is_continuous`（`event.rs:315-320`，删 `direct` 后无生产调用方）。
- **已删除（P1）**：`scroll::scroll_ancestor_on`（原 `interaction/scroll.rs:63-70`）及其在 `text_edit.rs` 的调用；P4 不再处理该项。
- **裁决 C 删除（滚动动画能力）**：`scroll_to_animated`（`app/scroll.rs:207-238`）、`AnimationTarget::Scroll`、`scroll_animation_target`（`app/animation.rs:15-29`）与 `App::scroll_animation_target` 包装（`app.rs:1005-1007`）全部删除；程序化滚动统一立即 `scroll_to`。
- 旧断言测试（§2.4 明确列出）与相关测试辅助。
- **保留**：`scroll::scroll_ancestor` / `scroll_ancestors` 作为 text_edit/a11y 的结构性（非代理）助手保留——它们仍被 `text_edit.rs:3544/4132/4158` 与 `a11y/mapping.rs:297` 使用；本批只把滚轮/键盘/a11y 的“找滚动目标”迁移到 `scroll_candidates`，不强行删除底层遍历器。若 P4 后发现可统一再删。

### 5.4 依赖与错误

- 不新增 crate；`ScrollAxes` 用普通枚举，不引入 `bitflags` 也足够（`bitflags` 已在依赖图内，但避免为二值集合引入新习惯）。
- 错误：窗口/节点/目标非滚动容器复用 `EngineError::{UnknownWindow, StaleNode, NotScrollable}`；**自指或后代目标新增 `EngineError::InvalidScrollProxy { node, target }`**（语义明确的强类型错误；不复用 `NotScrollable` 误导调用方）。
- `set_scroll_proxy` 校验顺序：窗口 → 节点 → 目标存在且拥有 `ScrollSlot` → 自指拒绝 → 后代目标拒绝（`tree.is_ancestor(node, target)`）；任一步失败不产生半写状态。
- `ScrollBar::attach` 改为 `Result<(), EngineError>`（现有调用点多为 `let _ = ...` 或 `.expect`，由调用方处理）；`release` 内部只忽略 `StaleNode`。
- 不给 `ScrollConfig`/`ScrollState` 增加“策略/优先级”字段（D5 未选）。

### 5.5 调用点迁移清单（编译期穷举）

| crate / 文件 | 迁移内容 |
|---|---|
| `ui_core::interaction.rs` | `ScrollAxis` 定义删除；`ScrollConfig`/`ScrollState`/`ScrollSlot` 字段换 `axes` |
| `ui_core::interaction/scroll.rs` | `max_offset`/`clamp_offset` 双轴；`scroll_ancestor_on` **已删除（P1）**；测试槽构造换 `axes` |
| `ui_core::app/input.rs` | **P1 就要改**（否则删 `ScrollAxis` 后不编译）：`on_scroll`/`default_key_scroll` 的单轴 match 机械改为 `axes`（旧链式保留）；P2 再整体重写 `on_scroll`，并把 `InputEvent::Scroll.timestamp` 透传到 `App::on_scroll`（当前 `input.rs:167-182` 用 `..` 丢弃）；**模块保持私有**，仅窄导出 `refresh_hover_under_pointer`（`pub(crate)`）供 `interaction/scroll.rs` |
| `ui_core::app.rs` | `App::on_scroll` 签名加 `timestamp`；删除 `App::scroll_animation_target` 包装（`app.rs:1005-1007`） |
| `ui_core::app/scroll.rs` | `make_scroll` 校验/写入 `axes`；`scroll_to` 双轴夹紧；`ScrollState.axes` |
| `ui_core::app/virtual_list.rs` | `VirtualListConfig.content_width`；`ScrollSlot { axes: Vertical | Both }`（`content_width=Some` 时 Both）；三处 `scroll.content` 写入保留横向内容宽（`content_width.unwrap_or(viewport.width)`） |
| `ui_core::app/animation.rs` | 删除 `scroll_animation_target` **与 `AnimationTarget::Scroll`**（滚动动画能力删除，裁决 C）；动画系统仅保留 UI 动画（`AnimatedValue` 等） |
| `ui_core::text_edit.rs` | **P1 已收口**：`scroll_ancestor_on` 已删除（P1），reveal/ensure-visible 改走 `scroll::scroll_ancestors` + 轴过滤；**单行 `Both` 横向优先、多行 `Both` 纵向优先**（`text_edit.rs:3580-3598` 的 preferred 顺序、`:4216`/`:4234` 的 reveal 分支）；多行 `Both` 若横向 overflow 也 reveal 横向（P1 收口）；**生产 `Input` 仍只建单轴槽**（单行 `Horizontal` / 多行 `Vertical`），`Both` 仅出现在测试/未来双轴输入；`match slot.axis` 已改 `axes` |
| `ui_core::a11y.rs`、`a11y/{tree,adapter}.rs` | `A11yScroll.vertical` → `orientation: Option<A11yOrientation>`；`scroll_state` 按 `axes` 计算；adapter 仅 `Some` 时设置；测试槽构造 |
| `ui_core::a11y/action.rs` | 横向动作步长改用 `viewport.width`（`455-458` 现为 height，既有缺陷）；`a11y_scroll_target` 按动作轴过滤 |
| `ui_core::a11y/mapping.rs:297` | 保留 `scroll_ancestor` 的结构性用法；不强制迁移（见 §5.3 保留项） |
| `ui_core::platform/wayland/scroll.rs`、`platform/linux/surface.rs` | **排除**：Wayland 有自己的私有的 `ScrollAxis`（`wayland/scroll.rs:8`，60+ 处），是协议级单轴概念，不迁移、不重命名、不纳入 `rg` 残留检查 |
| `ui_core::tests/layout.rs`、`app/*_tests.rs`、`paint/pass.rs`、`test_support/gui_smoke.rs` | `ScrollConfig { axis }` / `ScrollSlot { axis }` 字面量改 `axes`（约 32 个 `ScrollConfig`、12 个 `ScrollSlot`、3 个 `ScrollState.axis` 读点） |
| `ui_kit::kit/input.rs` | **保留 `FieldAxis`**（`SingleLine/Multiline` 是布局判别器，`input.rs:180-194/246-384`，不能替换）；只改 2 个 `ScrollConfig` 字面量（`648-658`）、`state.axis` 判断（`856`）改 `axes`、删除 `scrollbar_axis` 辅助（`953-958`）改用 `ScrollbarAxis::Vertical`；clear 代理见 §4.7 |
| `ui_kit::kit/{list,table,tree,menu,dropdown}.rs` | 纵向配置改 `axes: Vertical`；Table header 代理（Geometry 增加 header） |
| `ui_kit::kit/scroll_bar.rs` | `attach -> Result` + track 代理接线/清理；`ScrollbarAxis::point(current, offset)` 保留另一轴（`54-59/460-462/510-512`） |
| `ui_kit/examples/gallery.rs` | `state.axis` 的 probe（`:2625`）改 `state.axes`（**P1 已迁移**）；⑦ 改双轴 + 两条 bar、外层页面可见 bar、`--wheel-check`（P3-3/P3-4） |
| `settings_ui/src/layout.rs`、`updater_ui/src/dialog.rs`、`widgets/candlestick/...`、`widget_gallery_ui/src/gallery.rs` | **P1 不需要改**（它们只用 `ScrollConfig::default()`/`Default::default()`，无 `axis` 字面量）；P3 只做各自 bar 的代理接线 |
| `ui_core/benches/perf.rs`、`examples/acceptance/*` | 若使用 `ScrollConfig` 字面量则同步 |

> 迁移以编译器错误为清单，禁止 `#[allow]`/`#[expect]` 绕过；不写兼容层。`rg` 残留检查必须限定 `interaction::ScrollAxis` 并排除 `platform/wayland`/`platform/linux/surface.rs` 的合法同名枚举。

## 6. 实施计划（分阶段）

### P0 — 证据固化与红测（0.5 天）

| 任务 | 产出 | 验收 |
|---|---|---|
| P0-1 复现探针入库外证据 | 保留 `E:\kytrix-scratch\scrollprobe` 与真实窗口截图；结论已写入 §2.2 | 证据路径登记附录 A |
| P0-2 写失败用例 | **仅写能在当前 HEAD 编译的行为用例**：单目标闩锁（**E4–E6 红测、E7 绿色守卫**）、边界（E4/E5）、对角（E12 旧断言）、`wheel_events_in_one_frame_accumulate` 语义确认、kit 的 bar/track 用例（K1/K2）；gallery `--wheel-check` 骨架（先只打印值和 FAIL）。**代理/双轴用例（E10/E11/E14–E16/K3–K7）必须等 P1/P2 的类型/API 落地后再写**，不得为让测试编译而加临时死代码。 | **红测先行并由 `#[ignore]` 隔离：E2/E4/E5/E6 于 P2 移除、K1/K2 于 P3 移除**；P0 默认门禁绿，`--ignored` 复现红；P1/P2/P3 按 §7.1/§7.2 转绿并记录 commit |
| P0-3 基线记录 | 记录 `cargo test -p ui_core`/`-p ui_kit` 基线、gallery `--idle-check`/`--probe` 输出 | 附录 A 基线表 |

**文件作用域**：`crates/ui_core/src/app/interaction_tests.rs`、`crates/ui_kit/src/kit/{scroll_bar,list,table,input}.rs`（仅测试模块）、`crates/ui_kit/examples/gallery.rs`（仅新增 flag/探针，不接线）。

**依赖**：无。**门禁**：红测列表 + 基线日志；`cargo fmt`。

### P1 — 双轴滚动模型（1.5 天）

| 任务 | 产出 | 验收 |
|---|---|---|
| P1-1 `ScrollAxes` + `ScrollSlot`/`ScrollConfig`/`ScrollState` 双轴 | 引擎类型与 clamp/offset 逻辑 | 双轴 clamp/禁用轴/默认纵向单测 |
| P1-2 全部调用点迁移 | §5.5 清单编译通过；**必须包含 `app/input.rs` 的机械 `axes` 迁移**（旧链式保留）；旧单轴测试改 `axes` | `cargo test -p ui_core` / `-p ui_kit` 绿（滚轮行为不变） |
| P1-3 文本框/虚拟列表/kit 适配 | `ensure_visible` 仍按轴工作；虚拟列表纵向；`ScrollBar` 双轴 `point` 保留另一轴 | 既有 text-edit/list/scroll_bar 用例全绿 |
| P1-4 a11y 双轴 | `A11yScroll.orientation` 按 axes；Both 不自动；横向步长改 width | a11y 用例 + Both/横向 step 用例 |
| P1-5 双轴 List 承载（addendum §3.5，增量） | `VirtualListConfig.content_width`、`VirtualSlot.content_width`、`app/virtual_list.rs` 三处 content 写入保留宽度、`ListOptions.content_width`、行 `width_full + min_width(content_width)`；`Some` 时 `ScrollSlot.axes = Both`，`None` 时 `axes = Vertical` 且行宽行为与 HEAD 一致 | 既有 list/virtual 用例全绿；`content_width=None` 时 `axes=Vertical`、行宽行为不变 |
| P1-6 滚动槽语义标记（addendum §3.5，按 AD3 **取消**） | AD3 sticky + AD2 自管两 bar 后 `ScrollSlot.a11y` 无使用点 ⇒ **不新增引擎字段**（`ScrollSlot`/`ScrollConfig` 保持 P1-1 的形状） | 无新增 API；复核 `ScrollSlot` 字段清单与 AD3 前一致 |

**文件作用域**：`crates/ui_core/src/{interaction.rs,interaction/scroll.rs,app/input.rs,app/scroll.rs,app/virtual_list.rs,text_edit.rs,error.rs,a11y.rs,a11y/*}`（`error.rs` 新增 `InvalidContentWidth`）、`crates/ui_kit/src/kit/{input,list,table,tree,menu,dropdown,scroll_bar}.rs`（类型迁移 + `content_width` 通道 + `scroll_bar` 的 point 改动）、`crates/ui_kit/examples/gallery.rs`（`state.axes` probe 迁移）、`ui_core/examples`/`benches`；测试文件同步。**不包含** `settings_ui`/`updater_ui`/`candlestick`/`widget_gallery_ui`（无 `axis` 字面量；其 `Column::flex` 迁移在 P3-10）。

**依赖**：P0。**门禁**：`cargo test -p ui_core -p ui_kit`、`clippy -D warnings`、`fmt`；**本阶段不得改变滚轮行为**（旧链式仍在），只做模型迁移。

### P2 — 路由内核（2 天）

| 任务 | 产出 | 验收 |
|---|---|---|
| P2-1 候选解析 + 代理 | `scroll_candidates`（visited/hops/失效代理校验）、`NodeTree::scroll_proxy`、`NodeInteraction.scroll_proxy`、`App::set_scroll_proxy`（含 `InvalidScrollProxy`） | 引擎单测：代理命中、代理链到祖先、**祖先→后代代理环终止**、失效代理跳过、自指/后代拒绝、可见/禁用目标跳过 |
| P2-2 序列会话 + 单目标闩锁 | `ScrollSession` + `route_wheel`；`on_scroll` 重写；**`timestamp` 从 `dispatch_input_tracked` 透传到 `App::on_scroll`**；phase 映射（含 End→Momentum 宽限、零 delta End/Cancel） | E4–E9、E23、E24、E27–E30 绿；旧链式用例按 §2.4 删除/替换；**删除 E2/E4/E5/E6 的 `#[ignore]`，默认 `cargo test` 必须运行并通过，`--ignored` 不再出现这 4 条（K1/K2 由 P3 删除）** |
| P2-3 立即消费 + 双轴 | 目标内两轴**一次** `scroll_to(goal)`（`goal = offset + applied`，符号归一化见 P2-6；不重置另一轴）；删除动画分支 | E1–E3、E12–E16、E31 绿；**同 P2-2 的 ignore 清理门禁：E2/E4/E5/E6 在 P2 删除 `#[ignore]`，默认 `cargo test` 必须运行并通过，`--ignored` 不再出现这 4 条（K1/K2 由 P3 删除；合计 6 条最终全部消失）** |
| P2-4 生命周期清理 | 浮层开/关、窗口失焦、节点卸载、指针捕获；代理目标删除 | E9/E9b/E23 + 验收；无 stale 访问 |
| P2-5 hover 重算 | `refresh_hover_under_pointer`；仅实际滚动后调用；指针捕获期间跳过 | E19、E29 绿；I3 不回归 |
| P2-6 45° 内核（addendum §2，增量） | `interaction/scroll.rs` 的 `request_px`（逐候选/目标 `line_height` + 符号归一化）、`applied_delta`、`smallest_angle_between`、`project_vector`、`apply_scroll`；`route_wheel` 用首事件 raw delta 做逐候选 hint；`consumed` 记账 | E34–E38、E34m/E34n 绿；E12 按“偏移不变、consumed 记账”改写；E4/E5 保持 |
| P2-7 零 delta/相位（addendum §2.6/§2.7，增量） | 零 delta 不建立会话；`End/Cancel` 先处理相位再判零 delta；`requested = -raw×line_height` 只在路由内部 | E8、E28 扩断言 |

**文件作用域**：`crates/ui_core/src/interaction/scroll.rs`（`scroll_candidates`/`follow_proxy_chain`/`route_wheel`/`ScrollSession`/45° 纯函数）、`app/input.rs`、`app.rs`、`app/animation.rs`、`app/scroll.rs`、`app/overlay.rs`、`app/window_set.rs`、`app/removal_tests.rs`（裁决 C 删除滚动动画槽用例）、`interaction.rs`、`interaction/animation.rs`（裁决 C 删除 `lerp_point`）、`node/tree.rs`、`node/tree/content.rs`、`error.rs`（`InvalidScrollProxy`）、`event.rs`（删除 `is_continuous`）、`app/interaction_tests.rs`。**不新建模块/结构体层**（45° 函数与路由同文件）。

> **P2 测试计数/分类（最终实测；证据：`E:\kytrix-scratch\scroll-impl\P2\README.md` + `E:\kytrix-scratch\research\review-p2-final.md`）**：`ui_core` lib **2736 passed / 1 ignored**、`ui_kit` lib **769 passed / 2 ignored**；相对 P1 基线（2691 passed / 5 ignored）为 **+45 passed、−4 ignored**；相对修复前 P2（2728/1）为 **+11 新增测试函数、−3 裁决 C 删除**。P2 清单：原 **34 项** → 裁决 C 删除 2 项（E31、程序化动画取消改写）→ **32 项**；另删除 1 条 P2 前既有用例（`removal_tests::scroll_animations_are_cancelled_with_their_container`，不在 34 项内）；修复轮新增 **11 条**（原 9 条 + 收口轮 2 条）⇒ 最终 **32 + 11 = 43 项新增/转绿**。类别按 README 标注 **existing / unignored（E2/E4/E5/E6）/ rewrite（E8/E9 等）/ new**。`--ignored` 剩余为 K1/K2（P3）+ 既有 dark-mode 1 条 + `tests/gui_smoke.rs` 1 条；`--all-features --lib -- scroll wheel` 回归通过（以 `all-features-test.log` 为准）。P2 README 偏差 1（`scroll_to_animated` 公开）随裁决 C 作废——**不保留该 API**。

**依赖**：P1。**门禁**：全部引擎用例绿；`--idle-check` 无额外帧；clippy/fmt。

### P3 — kit 接线与 gallery 验收（1 天）

| 任务 | 产出 | 验收 |
|---|---|---|
| P3-1 ScrollBar 代理 | `attach -> Result` 设 track 代理；`release` 清除；`ScrollbarAxis::point(current, offset)` 保留另一轴 | K1/K2/K6/K7b/K8 绿；**删除 K1/K2 的 `#[ignore]`，默认 `cargo test` 必须运行并通过，`--ignored` 不再出现 K1/K2（E2/E4/E5/E6 已在 P2 删除；合计 6 条全部消失）** |
| P3-2 Table header / Input clear 代理 | Table header 代理接线（**header 节点由 P3-7 创建**，本项只 `set_scroll_proxy` + K4 验收；`Geometry.header` 由 P3-7 记录）；clear 在 `set_clearable` 生命周期接线（不在 `install_scroll`） | K3/K4/K5/K10 绿 |
| P3-3 双轴演示 + 页面 bar | gallery ⑦ 双轴 + 两条 bar；外层页面可见 bar | 真实窗口两轴可滚、拖垂直条 x 不变、拖水平条 y 不变、页面 bar 与内容同步 |
| P3-4 `--wheel-check` | 进程内注入滚轮，断言内容/条/边界/双轴/hover；失败统一走退出码 3；**必须在 `--idle-check` 基线（1 s）之前完成并在基线前排空其滚动/淡入帧** | 三端（至少 Windows）PASS；与 `--idle-check` 分进程运行；组合模式（若加）有明确时序 |
| P3-5 应用层 bar 接线 | settings/updater/candlestick/widget_gallery | 编译 + 各自最小用例/手测 |
| P3-6 `Column` API（addendum §3.3/§3.10，AD4 增量） | `ColumnWidth::Flex { weight, min_width }`、`Column::flex(header, weight, min_width: LogicalPx)`（无默认/无链式）、`contribution()`、`span_cell_style` 重写、`TableInner.content_width` | K11/K12 绿；17 处 `Column::flex` 全部显式给值；table 宽度断言重算 |
| P3-7 Table sticky + 双 bar（addendum §3.4/§3.6/§3.8，AD2/AD3 增量） | outer/header/header_content 结构、sticky 写（`sticky_x` + `NodeTree::set_style`）、List `scrollbar:false`、Table 自管垂直/水平 bar + rail 尾隙、`Table::sync` 聚合 | K13–K17、K19、K21 绿；K9 几何守卫不回归 |
| P3-8 代理扩展（addendum §3.4，对 P3-1/P3-2 的增量） | 两条 bar 的 track 代理到 list viewport（P3-1 的 `attach` 已接线）；header 代理下横向滚轮也生效；确认表头无自身 `ScrollSlot` | K15/K16/K18、K4 保持 |
| P3-9 gallery 宽表（addendum §4，增量） | ⑩ 宽表（窄 420 / 宽 1100 对照，三参数列配置）；G7–G9 **追加到 P3-4 的 `--wheel-check`** | 三端至少 Windows 真机 PASS |
| P3-10 调用点迁移（addendum §3.10，AD1/AD4 增量） | 17 处 `Column::flex` 显式最小宽（widget 按旧密度取值并逐点记录）、`ListOptions`/`VirtualListConfig` 字面量、Table bar 归属；**与 P3-6 同提交**（避免两次迁移） | 编译零 warning；各 widget 最小用例/手测；`TableOptions` 不加字段 |

> 编号对齐：本表 P1-5/P1-6、P2-6/P2-7、P3-6…P3-10 与 `scroll-routing-addendum.md` §5 一一对应，无重复编号。依赖顺序：P3-7（节点结构）→ P3-2（header 代理）/P3-8（bar 代理验收）；P3-6 与 P3-10 同提交；P3-9 的检查项挂在 P3-4。

**文件作用域**：`crates/ui_kit/src/kit/{scroll_bar,list,table,input,tree,menu,dropdown}.rs`（含测试；`table.rs` 为 sticky/双 bar/Column API 主要落点，`list.rs` 为 `content_width` 通道）、`crates/ui_kit/examples/gallery.rs`（目录 ⑩ + `--wheel-check`）、`crates/ui_kit/Cargo.toml`（dev-dependency 增加 `automation` feature，仅测试/示例构建）、**addendum 迁移**：`crates/widgets/{force_order,financial_index,financial_calendar,hyperliquid_wallet,market_trade,watch_list}/**`、`crates/settings_ui/src/pages/proxy/table.rs`（仅 `Column::flex` 第三参数与按需 min_width；不改其滚动接线）、应用层各自 scroll 接线文件。

> **ignore 清理合并门禁（P2+P3）**：默认 `cargo test` 必须运行并通过，`--ignored` 不再出现 E2/E4/E5/E6/K1/K2 共 6 条（P2 删 4 条、P3 删 2 条）；每阶段删除后立即跑默认与 `--ignored` 两套并记录 commit。

**依赖**：P2。**门禁**：kit 用例绿；gallery `--wheel-check` PASS；真实窗口手测清单通过。

### P4 — 统一解析、收尾与三端验证（1 天）

| 任务 | 产出 | 验收 |
|---|---|---|
| P4-1 键盘/a11y 统一 | 共享 `scroll_candidates`（按动作轴过滤）；键盘语义不变；`a11y/action.rs` 横向步长改 width | E17/E18/E32/E32b/E33 绿；既有键盘/树/a11y 用例全绿 |
| P4-2 删除遗留 | 单轴枚举、`scroll_animation_target` + `App` 包装、**`AnimationTarget::Scroll`、`scroll_to_animated`（裁决 C）**、`is_continuous`、旧链式、过时测试（`scroll_ancestor_on` **已在 P1 删除，P4 不再检查**） | 限定 `rg 'interaction::ScrollAxis'` 无残留（排除 Wayland 同名枚举）；`rg 'scroll_ancestor_on|scroll_to_animated|AnimationTarget::Scroll'` 仅允许历史文档；编译零 warning |
| P4-3 文档回写 | 本文件状态/偏差；`custom-gui-engine-design.md` §1.3 API + §8.4 滚动段 + E22 状态（`:140/:4780/:5013`）；`.docs/.../kit-gallery.md` | 文档引用源码行号核对；E22.md 缺失引用修复或创建 |
| P4-4 三端验证 | Windows 125%（必须）；macOS/Linux（可用则跑） | §10 清单；证据归档 |
| P4-5 变异自检 | 恢复关键旧代码 → 对应用例红；还原 → 绿（含代理环、End→Momentum、ScrollbarAxis::point） | 变异日志 |

**依赖**：P2/P3。**门禁**：`cargo test --workspace`、`clippy -D warnings`、`fmt`、gallery 三模式（`--wheel-check`/`--idle-check`/`--probe`）。

### 6.1 关键路径与并行

```text
P0 ─▶ P1 ─▶ P2 ─▶ P3 ─▶ P4
                 └────▶ P4-3（文档）/ P4-4（真机）可提前准备
```

P1 的调用点迁移是机械但面广的步骤，必须独立提交、先保绿；P2 才改变行为。P3 的代理接线依赖 P2 的 API，但 UI 层的双轴演示可在 P1 后先搭骨架。

## 7. 测试计划

### 7.1 红→绿矩阵（引擎）

| ID | 用例 | 断言 | 当前 |
|---|---|---|---|
| E1 | `wheel_over_inner_content_scrolls_only_inner` | 内层 3 行 × 1 序列 = 144；外层 0 | 绿（守卫） |
| E2 | `line_wheel_applies_immediately_and_accumulates_in_one_frame` | 同帧 2 次 = 96，且无动画 | **红**（P0 已 `#[ignore]`，P2 转绿） |
| E3 | `pixel_wheel_applies_immediately` | 30 px 立即；独立 Pixel 立即守卫（不依赖行滚轮路径） | 绿（守卫） |
| E4 | `inner_at_end_does_not_move_outer_in_same_sequence` | 内层 max 后同序列继续 → 外层 0 | **红**（P0 已 `#[ignore]`，P2 转绿） |
| E5 | `new_sequence_after_idle_moves_outer_when_inner_at_end` | 组合 D1+D3：第一段同序列 → 外层 0；第二段间隔 > IDLE → 外层立即 48 | **红**（P0 已 `#[ignore]`，P2 转绿） |
| E6 | `wheel_sequence_latches_target_when_pointer_moves` | 指针移到页面后同序列继续 → 仍是内层 | **红**（P0 已 `#[ignore]`，P2 转绿） |
| E7 | `new_sequence_after_idle_retargets_to_hovered_container` | 间隔 > IDLE + 指针在页面 → 外层动 | 绿（守卫，旧实现已满足） |
| E8 | `continuous_phases_bound_sequence` | Begin 新序列；Update/Momentum 保持；**End 只置 ended_at**；Cancel 清理；零 delta End/Cancel 也生效 | **红** |
| E9 | `removing_latched_container_mid_sequence_does_not_panic` | 下一事件重新仲裁；无 panic/无 stale | **红** |
| E9b | `removing_proxy_target_falls_back_to_ancestor` | 代理目标删除后下一滚轮落到外层，不 panic | **红** |
| E10 | `wheel_over_scroll_proxy_scrolls_target` | 代理节点 → 目标容器；外层 0 | **红** |
| E10b | `proxy_cycle_with_ancestor_target_terminates` | A 祖先→B 后代、A→B→C→A 均终止且不滚错 | **红** |
| E11 | `wheel_proxy_target_at_end_chains_to_ancestor_on_new_sequence` | 新序列后外层动 | **红** |
| E12 | `diagonal_wheel_goes_to_first_consumable_inner` | 横内层吃 dx=12，外层 0（单赢家，不拆轴；不引入“主分量”规则） | 绿（旧断言当前绿；P2 改写为单赢家后转红） |
| E13 | `vertical_wheel_over_horizontal_inner_scrolls_outer` | 横内层不能消费 dy → 外层动 | 绿（守卫） |
| E14 | `dual_axis_container_consumes_both_axes` | dx、dy 都变 | **红**（新能力） |
| E15 | `dual_axis_clamps_each_axis_independently` | 一轴到边界不影响另一轴；**从非零 x/y 只滚一轴时另一轴保持** | **红** |
| E16 | `disabled_axis_stays_zero` | content 再大也不动 | **红** |
| E17 | `keyboard_scrolls_focused_container_through_proxy` | 焦点在 bar 上 → 滚容器（P4） | **红** |
| E18 | `a11y_scroll_actions_follow_axes_and_proxy` | 双轴/代理目标正确（P4） | **红** |
| E19 | `wheel_scroll_refreshes_hover_under_pointer` | hover 节点随滚动更新 | **红** |
| E20 | `wheel_without_movement_requests_no_frame` | 不唤醒/不出帧 | 绿（守卫，新代码不得破坏） |
| E21 | `wheel_outside_modal_scope_is_dropped` | 模态外不滚 | 绿（守卫） |
| E22 | `deeply_nested_candidates_no_panic` | 12 层嵌套、第 9 层才可滚时仍能滚到（不设 8 层截断） | **红** |
| E23 | `window_focus_loss_clears_scroll_sequence` | 失焦后再滚重新仲裁 | **红** |
| E24 | `scroll_config_axes_and_scroll_to_clamp_both_axes` | 双轴 API 正确 | **红** |
| E27 | `continuous_end_then_momentum_keeps_target` | End 后 Momentum 沿用目标，不重新仲裁（macOS 惯性） | **红** |
| E28 | `timestamp_sequence_boundaries_are_monotonic_and_robust` | 同 timestamp/回退 timestamp/粗粒度 timestamp 三组确定性行为 | **红** |
| E29 | `wheel_hover_refresh_skipped_during_pointer_capture` | 捕获期间不打断 hover；释放后恢复 | **红** |
| E30 | `scroll_sequences_are_isolated_per_window` | 两个窗口各自闩锁互不影响 | **红** |
| E31 | ~~`scroll_animation_is_cancelled_by_new_wheel_sequence`~~ | **已删除（裁决 C：滚动动画能力删除）**；P2 同步删除程序化动画取消用例，不再验收 | 删除 |
| E32 | `a11y_horizontal_steps_use_viewport_width` | ScrollLeft/Right 用 width；Both 不自动 orientation | **红** |
| E32b | `a11y_scroll_down_falls_through_horizontal_inner` | 横向内层 + 纵向页面：ScrollDown 落到外层（与 E13 一致） | **红** |
| E33 | `keyboard_both_axis_mapping_and_bar_focus_proxy` | Both 容器 Page/Home 轴映射；bar 焦点代理 | **红** |

> P0 实测口径（2026-11-26，证据见附录 A 的 P0 基线路径）：默认 `cargo test` 门禁绿；红测先行并以 `#[ignore]` 隔离，`--ignored` 复现红，P2 转绿。E2/E4/E5/E6 为已 `#[ignore]` 的红测；E7 旧实现已满足（守卫）；E12 旧断言当前绿、P2 改写为单赢家后转红；E5 覆盖 D1+D3 组合（第一段同序列外层 0，第二段 >IDLE 外层立即 48）。

### 7.2 红→绿矩阵（kit / gallery）

| ID | 用例 | 断言 | 当前 |
|---|---|---|---|
| K1 | `list_wheel_over_track_scrolls_list_not_page` | page 0，list 动 | **红**（P0 已 `#[ignore]`，P3 转绿） |
| K2 | `list_wheel_over_thumb_scrolls_list` | 同上 | **红**（P0 已 `#[ignore]`，P3 转绿） |
| K3 | `multiline_input_wheel_over_bar_scrolls_input` | input 动，page 0 | **红** |
| K4 | `table_header_wheel_scrolls_body` | body 动，page 0；`Table::Geometry.header` 已存 | **红** |
| K5 | `input_clear_button_wheel_scrolls_input` | 在 `set_clearable(true)` 后创建按钮的场景下，input 动、page 0 | **红** |
| K6 | `menu_panel_wheel_over_bar_scrolls_panel_list` | 面板列表动 | **红** |
| K7 | `dual_axis_demo_bars_scroll_both_axes` | 两条 bar 各自轴生效 | **红** |
| K7b | `dual_axis_scrollbar_preserves_other_axis` | 拖垂直条 x 不变；拖水平条 y 不变 | 绿（P1 已加 ScrollBar 组件级 track click + thumb drag 测试；P3 仍补 Table/代理完整 K7b） |
| K8 | `scrollbar_release_clears_proxy_and_is_safe` | 重复 release 幂等；卸载后不 panic | **红** |
| K9 | `scrollbar_geometry_unchanged_by_proxy` | thumb/track 几何与旧断言一致 | 绿（守卫） |
| K10 | `proxy_target_disabled_or_hidden_is_skipped` | track 可见、target 禁用/隐藏时不滚目标，继续向外层 | **红** |
| G1 | `--wheel-check`：内容点 | 只滚内层 | **红** |
| G2 | `--wheel-check`：条/表头/clear 点 | 只滚所属容器 | **红** |
| G3 | `--wheel-check`：边界 | 同序列不滚外层；停顿后滚外层 | **红** |
| G4 | `--wheel-check`：hover | 滚动后 hover 更新 | **红** |
| G5 | `--wheel-check`：双轴 | ⑦ 两轴都可滚 | **红** |
| G6 | `--wheel-check` 后独立运行 `--idle-check` | 无额外帧 | 守卫（分进程） |

> K7b 分两段：P1 已覆盖 ScrollBar 组件级 track click / thumb drag 的“保留另一轴”行为（`ScrollbarAxis::point(current, offset)`），状态转绿；P3 仍补 Table 双轴 + 代理链路的完整 K7b（拖垂直条 x 不变、拖水平条 y 不变，含 header 镜像偏移与双 bar 归属）。

### 7.3 手工验收矩阵（真实窗口）

| ID | 环境 | 步骤 | 期望 |
|---|---|---|---|
| M1 | Windows 125%（本机） | gallery `--scroll-to=1000`：在 ⑦ 内容区、⑦ 滚动条、⑥ 列表行、⑧ 表头、⑨ 树行、页面空白分别滚 1 刻度 | 内容区/列表/树滚自身；条/表头滚所属容器；页面空白滚页面；页面不因内层滚动而移动（同序列） |
| M2 | 同上 | 内层滚到底后停 500 ms 再滚 | 外层页面开始滚（新序列） |
| M3 | 同上 | 指针在 ⑦ 内但快速移到页面后继续滚（同序列） | 仍滚 ⑦（闩锁） |
| M4 | 同上 | ⑦ 双轴演示：竖滚、Shift+滚、横向滚 | 两轴各自正确；不存在“先试竖再试横”的乱滚 |
| M5 | 同上 | 打开 dropdown/menu，在面板内/外滚 | 面板内滚列表；面板外页面不动（modal scope） |
| M6 | 同上 | 键盘：聚焦列表/树后方向键、Page、Home/End | 焦点容器滚动；bar 聚焦时代理到容器 |
| M7 | macOS / Wayland（可用则跑） | 触控板双指滚动、相位 Begin/Update/End | 序列锁定、到边界不中途换目标；像素滚动立即 |
| M8 | 全部 | `--idle-check` | PASS（滚动条静止不出帧） |

### 7.4 变异自检（必须）

| 变异 | 预期红 |
|---|---|
| 移除 `scroll_candidates` 的代理优先级 | E10/E11/K1/K4 |
| 移除 `visited`/环防护 | E10b |
| 移除序列闩锁（每事件重新 `hit_at`） | E6/E7 |
| 恢复“逐祖先消费余量” | E4/E12 |
| ~~行滚轮恢复动画分支~~（能力已删除，裁决 C，不再作为变异项） | 由 E2/E3 直接守卫“立即语义”；变异改为“`scroll_to` 延迟生效” → E2/E3 红 |
| 恢复单轴 `clamp_offset` | E14/E15 |
| `End` 立即清会话 | E27 |
| `ScrollbarAxis::point` 清零另一轴 | K7b |
| 移除滚动后 hover 刷新 | E19/G4 |
| 移除 target 有效性检查 | E9/E9b |
| 代理目标不做可见/禁用过滤 | K10 |

记录方式：变异 → `cargo test` 红 → 还原 → 绿；日志与 SHA256/命令归档到 `.review/`（本机 gitignored）或本次证据目录。

### 7.5 不变量/性能门禁

- **I3**：`--idle-check` 在滚动条静止时不得出帧（沿用 `ui_kit/src/kit/scroll_bar.rs:1266-1279` 与 gallery `--idle-check`）。
- **I7**：`wheel_without_movement_requests_no_frame`、`same_offset_is_a_total_noop` 保持绿。
- **绘制**：滚轮只改 offset；`a_one_pixel_scroll_frame_rebuilds_nothing_and_damages_only_the_viewport`、`scrolling_moves_the_offset_into_the_slot_table_without_touching_lanes` 保持绿；嵌套滚动仍走完整遍历（既有行为，不改）。
- **悬停**：`update_hover` 只对 hover 链 diff 标 `bind/paint`；不引入全量样式重算（`scroll_never_reevaluates_bind_style` 保持绿）。
- **命中**：滚轮事件新增的命中测试次数：新序列 1 次 + 实际滚动后 hover 刷新 1 次；序列内 0 次（闩锁收益）。

## 8. 迁移与删除清单

### 8.1 删除

- `interaction::ScrollAxis` 枚举及 `ScrollSlot.axis`/`ScrollConfig.axis`/`ScrollState.axis`（P1 已删除）。
- `interaction/scroll.rs::scroll_ancestor_on` **已删除（P1）**；`scroll_ancestors` 保留为结构性助手（text_edit/a11y 仍用，见 §5.3）。
- `app/animation.rs::scroll_animation_target`、`AnimationTarget::Scroll`（裁决 C）。
- `app/scroll.rs::scroll_to_animated` 与 `App::scroll_to_animated` 包装（裁决 C：滚动动画能力删除，程序化统一立即 `scroll_to`）。
- `app/input.rs::on_scroll` 内联 `chain`/`direct`/动画分支（`609-682`、`605`、`639-646`）。
- 旧测试：`a_two_dimensional_wheel_delta_is_split_across_the_scroll_chain`（替代为 E12）、`pixel_scroll_follows_immediately_while_line_scroll_is_animated`（替代为 E2/E3）、`programmatic_scroll_cancels_a_pending_wheel_animation`（**删除**，裁决 C：滚动动画能力删除；E31 一并删除）。
- 任何为单轴假设服务的死代码/匹配分支。

### 8.2 文档回写

| 文档 | 内容 |
|---|---|
| 本文件 | 状态、用户裁决回填、审查记录（附录 C）、实施偏差（附录 D） |
| `crates/ui_core/design/custom-gui-engine-design.md` | §1.3 API 清单（`ScrollAxes`/`set_scroll_proxy`/`ScrollConfig` 新形状）；**§8.4（`:2417-2424`）滚动机制段**更新为目标解析/闩锁/代理；E22 状态在 `:140`、`:4780-4781`、`:5013` 三处更新；`.docs/gui-migration/spec-deltas/E22.md` 当前不存在（`:4781` 引用失效），创建或删除失效引用 |
| `crates/ui_core/README.md` | 公共 API 增量与滚动语义一段（如该文件维护 API 摘要） |
| `.docs/gui-migration/spec-deltas/kit-gallery.md` | ⑦ 演示改为双轴 + 页面滚动条的验收钩子/证据更新 |
| 提交说明 | 每阶段语义变更、被删除的旧断言、迁移原因（AGENTS 要求历史写提交说明/文档，不写代码注释） |

### 8.3 不做兼容

- 不保留 `ScrollAxis` 别名、不保留 `ScrollConfig::axis` 字段、不写 migration/fallback。
- 应用层调用点直接改到新形状；编译器错误即清单。

## 9. 风险与开放问题

### 9.1 风险

| ID | 风险 | 影响 | 缓解 |
|---|---|---|---|
| R1 | Chrome 式闩锁在慢速滚轮下（事件间隔 > 200 ms）退化为逐刻度新序列，边界处仍会“滚一下内层、再滚一下外层” | 手感差异，可能被认为“没锁住” | 真机调参 IDLE（150–300 ms）；gallery `--wheel-check` 覆盖慢速间隔；文档写明 |
| R2 | 行滚轮立即到位改变现有手感 | 用户感知 | D3 已裁决；gallery 手测确认；文本编辑 ensure-visible 不受影响（程序化） |
| R3 | 双轴迁移面广（约 120 处 `ScrollAxis` 引用 + 32 个 `ScrollConfig` 字面量 + 12 个 `ScrollSlot` 字面量；另约 60-70 处 Wayland 同名枚举**不迁移**） | 机械错误/遗漏 | P1 独立提交、编译器穷举、`rg 'interaction::ScrollAxis'` 复核、先保绿再改行为 |
| R4 | 代理环/悬空目标 | panic/滚错 | 目标必须是滚动容器、去重、限深、自指拒绝、E9/K8 |
| R5 | 表头/clear 按钮代理时序（viewport 尚未 attach） | 代理设不上 | 在 `Table::attach`/`Input::install_scroll` 后接线；K4/K5 |
| R6 | 滚动后 hover 刷新带来额外命中测试 | 性能/帧 | 仅实际滚动后一次；`--idle-check`/基准对比；必要时只在 hover 链可能变化时刷新（命中测试本身已是 O(log n)） |
| R7 | 双轴 `Both` 的 a11y orientation 无法自动推导 | 无障碍语义缺失 | 显式 `A11yPatch.orientation` 优先；Both 不自动设置；文档 + 测试 |
| R8 | 浮层/模态与序列清理不彻底 | 滚到被遮挡内容 | 浮层开/关、失焦、目标卸载清会话；E21/E23/M5 |
| R9 | 跨平台相位差异（Windows 无 phase，macOS/Wayland 有） | 序列边界不一致 | 统一用时间戳；phase 仅用于显式 Begin/End；M7 |
| R10 | 双轴演示的滚动条角落重叠 | 视觉瑕疵 | 用 `inset` 预留；不够则先在 demo 局部处理，不急着扩公共 API |
| R11 | 时间戳来源不一致：Windows `GetMessageTime`、macOS `NSEvent.timestamp`、Wayland `Instant::now()`（`platform/linux/surface.rs:2080-2098`） | 序列边界在 Wayland 上受事件生成延迟影响 | 统一用单调 `Instant` 差；`saturating_duration_since` 处理回退；Wayland 用事件创建时刻并在 R9 登记局限；E28 覆盖同刻/回退/粗粒度 |
| R12 | `Input` clear 按钮在 `install_scroll` 后才创建；disabled 输入无 `ScrollSlot` | 代理设不上或误设 | 在 `set_clearable` 生命周期接线；目标无槽时跳过；K5/K10 守卫 |
| R13 | `Table` 双轴横向滚动：原计划不在本批（仅引擎 + ⑦ 演示） | 用户可能预期表格横向滚动 | **已被 addendum A2 覆盖**：Table 横向纳入本批且全局默认（内容宽公式、Table 内部 sticky 表头、水平/垂直条尾隙几何、迁移与测试见 `scroll-routing-addendum.md` §1.2/§3）；本表其余风险不变 |

### 9.2 开放问题（终审确认）

| ID | 问题 | 本计划默认 | 需确认 |
|---|---|---|---|
| O1 | Chrome 的 45° 对角投影是否要精确复刻？ | **已被 addendum A1 覆盖：用户裁决精确复刻**（`scroll-routing-addendum.md` §2；偏移不变、消费记账按 Chrome 实现，见其 §2.3 澄清） | 已裁决，无需再确认 |
| O2 | 双轴水平行步长 | 复用 `line_height`（垂直/水平同一步长） | 是否要独立水平步长？ |
| O3 | 双轴交付边界 | **已被 addendum A2 覆盖：用户裁决 Table 横向纳入本批且全局默认**（内容宽 = Σ Fixed 宽 + Σ Flex 最小宽；`Column::flex(header, weight, min_width)` 三参数显式最小宽；Table 内部 sticky 表头；`scroll-routing-addendum.md` §1.2/§3） | 已裁决；**AD1–AD4** 已裁决关闭（addendum §6.3；与主计划 D1–D6 编号分离） |
| O4 | 程序化滚动是否也刷新 hover | 本批只做滚轮后刷新（避免 ensure-visible 高频路径） | 是否要一并做？ |
| O5 | 序列空闲阈值 | 200 ms 常量，真机调参后固定 | 是否接受该量级？ |
| O6 | gallery 页面滚动条样式/位置 | 右侧 overlay，复用 `ScrollBar`（Always 或 Hover） | 是否要与深色主题一致？ |
| O7 | Table header / Input clear 代理是否纳入本批 | 纳入（同属“装饰节点归属”缺陷族） | 是否同意？ |
| O8 | 滚动条 Hover 模式不可见时是否仍代理滚轮 | 是（命中区域仍属于容器） | 是否同意？ |
| O9 | 双轴容器的 a11y `Both` 是否要求强制显式 orientation？ | 是（不自动推导）；单轴自动 | 是否同意？ |
| O10 | `scroll_candidates` 的 `MAX_CHAIN_HOPS=32` 安全阀 | 32；不设候选数硬上限 | 是否接受？ |

## 10. 验收标准

### 10.1 命令门禁

```text
cargo test --workspace
cargo +nightly clippy -- -D warnings
cargo +nightly fmt --check
cargo run -p ui_kit --example gallery -- --wheel-check
cargo run -p ui_kit --example gallery -- --idle-check
cargo run -p ui_kit --example gallery -- --probe
```

- `--wheel-check`：PASS 时打印 `gallery: wheel-check PASS (...)`；任一断言失败打印 `gallery: wheel-check FAIL <case>` 并置 `failed = true`，与现有 `--idle-check`/`--theme-check`/`--dump-overlay` 统一走退出码 3。
- `--idle-check`：滚动条静止不出帧，PASS 退出码 0。
- `--wheel-check` 与 `--idle-check` 分两次进程运行（`--idle-check` 自带 3 s 关窗逻辑，不与其组合）。

### 10.2 行为验收（对应用户诉求）

1. 鼠标在任一内层滚动组件内容上滚轮：**只有该组件滚动**；外层页面在同一序列内不动。
2. 鼠标在该组件的滚动条/表头/清除按钮上滚轮：**滚该组件**，不滚外层页面。
3. 内层到边界后：本次序列停在边界；**停顿后再次滚动**由外层接手（Chrome 式）。
4. 连续滚动中移动鼠标到另一容器：序列内不换目标；新序列才换。
5. 双轴组件（gallery ⑦）：两个方向都能滚，两条滚动条同步。
6. 行滚轮**立即到位**，无 120 ms 延迟；同一帧多刻度累加。
7. 滚动后 hover/tooltip/滚动条淡入立即反映指针下的新内容。
8. 键盘/无障碍滚动目标与滚轮共用同一代理/双轴解析；键盘步进语义不变。
9. 静止时不出帧（I3）；无滚动量时不唤醒（I7）。
10. 三端构建与测试通过；Windows 125% 实机手测清单（M1–M6）通过；macOS/Wayland 有环境则跑 M7。

### 10.3 性能与不变量

- 滚轮热路径：新序列 1 次命中测试；序列内 0 次；实际滚动后 1 次 hover 刷新（仅当 hover 变化时标 bind/paint）。
- 无新增每帧开销；不改变绘制快路径判据（嵌套滚动仍退回完整遍历）。
- 嵌套深度上限 8 保持；候选/会话不持有绘制槽位号。
- 无 `#[allow]`/`#[expect]`/`dead_code` 绕过；无注释；无新依赖；无 `serde_json::Value` 等违反 AGENTS 的用法。

## 附录 A：证据索引

| 证据 | 位置/命令 | 结论 |
|---|---|---|
| TestApp 探针程序 | `E:\kytrix-scratch\scrollprobe`（仓库外；`cargo run` 输出 §2.2 A/B） | 内层内容优先正确；bar 命中外层；虚拟列表正确 |
| 真实窗口截图（DPI-aware） | `E:\kytrix-scratch\{g0b,g1_before,g2_after_inner,t0,t1_inner_one_notch,u0,u1_track_one_notch}.png` | 内容区滚内层；track 滚页面 |
| 真实窗口注入脚本 | `E:\kytrix-scratch\gallery.ps1`（`launch/capture/wheel/close`，`SetProcessDPIAware`） | OS 级 `WM_MOUSEWHEEL` 复现 |
| Chrome 源码级备忘 | `E:\kytrix-scratch\research\chrome.md` | 单目标闩锁、序列内不结转、滚轮=所属 scroller、45° 规则 |
| Qt 源码级备忘 | `E:\kytrix-scratch\research\qt.md` | Widgets 逐层冒泡/限位 ignore；Quick Flickable 边界冒泡；滚动条差异 |
| Flutter 源码级备忘 | `E:\kytrix-scratch\research\flutter.md` | resolver 单赢家、可消费才登记、整包消费、无结转 |
| 代码走查 | `E:\kytrix-scratch\research\codebase-survey.md` | 当前路径/缺口/结构/测试锚点（本文件 §2/§5 引用其行号） |
| **P0 基线（2026-11-26）** | `E:\kytrix-scratch\scroll-impl\P0\`（README + 默认/`--ignored`/基线日志 + gallery 输出） | 默认门禁绿：`ui_core` lib **2676 passed / 5 ignored**、`ui_kit` lib **763 passed / 2 ignored**；红测以 `#[ignore]` 隔离，`--ignored` 复现红（E2/E4/E5/E6 等），供 P2 转绿对照 |

## 附录 B：外部参考

**Chrome / Chromium**

- `cc/input/input_handler.cc`（`ScrollBegin` L136、`HitTestScrollNode` L1835、`ScrollLatchedScroller` L2195、`CanPropagate` L2336、`FindNodeToLatch` L2341、`CanConsumeDelta` L2462）：<https://source.chromium.org/chromium/chromium/src/+/main:cc/input/input_handler.cc>
- `third_party/blink/renderer/platform/widget/input/input_handler_proxy.cc`（`HandleMouseWheel` L1129）：<https://source.chromium.org/chromium/chromium/src/+/main:third_party/blink/renderer/platform/widget/input/input_handler_proxy.cc>
- `third_party/blink/renderer/core/input/mouse_wheel_event_manager.cc`：<https://source.chromium.org/chromium/chromium/src/+/main:third_party/blink/renderer/core/input/mouse_wheel_event_manager.cc>
- `components/input/mouse_wheel_event_queue.cc`：<https://source.chromium.org/chromium/chromium/src/+/main:components/input/mouse_wheel_event_queue.cc>
- `cc/input/overscroll_behavior.h`、`cc/trees/scroll_node.h`、`cc/input/README.md`：<https://source.chromium.org/chromium/chromium/src/+/main:cc/input/overscroll_behavior.h>
- `content/browser/renderer_host/input/mouse_wheel_phase_handler.cc`：<https://source.chromium.org/chromium/chromium/src/+/main:content/browser/renderer_host/input/mouse_wheel_phase_handler.cc>
- CSS Overscroll Behavior Level 1：<https://drafts.csswg.org/css-overscroll-behavior/>
- Chrome 73 根目标 wheel passive：<https://developer.chrome.com/blog/scrolling-intervention-2>

**Qt 6.8**

- `qapplication.cpp`（`QEvent::Wheel` 冒泡 L2813）：<https://code.qt.io/cgit/qt/qtbase.git/tree/src/widgets/kernel/qapplication.cpp?h=6.8#n2813>
- `qwidgetwindow.cpp`（`handleWheelEvent` L900）：<https://code.qt.io/cgit/qt/qtbase.git/tree/src/widgets/kernel/qwidgetwindow.cpp?h=6.8#n900>
- `qwidget.cpp`（`QWidget::wheelEvent` L9610、`QWidget::event` L8988）：<https://code.qt.io/cgit/qt/qtbase.git/tree/src/widgets/kernel/qwidget.cpp?h=6.8#n9610>
- `qabstractscrollarea.cpp`（`viewportEvent` L1049、`wheelEvent` L1170）：<https://code.qt.io/cgit/qt/qtbase.git/tree/src/widgets/widgets/qabstractscrollarea.cpp?h=6.8#n1170>
- `qabstractslider.cpp`（`scrollByDelta` L669、`wheelEvent` L732）：<https://code.qt.io/cgit/qt/qtbase.git/tree/src/widgets/widgets/qabstractslider.cpp?h=6.8#n732>
- `qscrollbar.cpp`（`wheelEvent` L475）：<https://code.qt.io/cgit/qt/qtbase.git/tree/src/widgets/widgets/qscrollbar.cpp?h=6.8#n475>
- `qquickdeliveryagent.cpp`（`deliverSinglePointEventUntilAccepted` L1334、`pointerTargets` L2108）：<https://code.qt.io/cgit/qt/qtdeclarative.git/tree/src/quick/util/qquickdeliveryagent.cpp?h=6.8#n1334>
- `qquickflickable.cpp`（`wheelEvent` L1621）：<https://code.qt.io/cgit/qt/qtdeclarative.git/tree/src/quick/items/qquickflickable.cpp?h=6.8#n1621>
- `qquickscrollbar.cpp`：<https://code.qt.io/cgit/qt/qtdeclarative.git/tree/src/quicktemplates/qquickscrollbar.cpp?h=6.8>

**Flutter stable `ae9b586f34a8`**

- `scrollable.dart`（`_receivedPointerSignal` L955、resolver 登记 L973）：<https://github.com/flutter/flutter/blob/ae9b586f34a8443573f82ef57c3bf914a908cbf7/packages/flutter/lib/src/widgets/scrollable.dart>
- `pointer_signal_resolver.dart`：<https://github.com/flutter/flutter/blob/ae9b586f34a8443573f82ef57c3bf914a908cbf7/packages/flutter/lib/src/gestures/pointer_signal_resolver.dart>
- `binding.dart`（signal 命中/结算 L407、L506、L536）：<https://github.com/flutter/flutter/blob/ae9b586f34a8443573f82ef57c3bf914a908cbf7/packages/flutter/lib/src/gestures/binding.dart>
- `scroll_position_with_single_context.dart`（`pointerScroll` L210）：<https://github.com/flutter/flutter/blob/ae9b586f34a8443573f82ef57c3bf914a908cbf7/packages/flutter/lib/src/widgets/scroll_position_with_single_context.dart>
- `nested_scroll_view.dart`（coordinator L956）：<https://github.com/flutter/flutter/blob/ae9b586f34a8443573f82ef57c3bf914a908cbf7/packages/flutter/lib/src/widgets/nested_scroll_view.dart>
- `scrollbar.dart`（`_receivedPointerSignal` L2362、`hitTest` L711）：<https://github.com/flutter/flutter/blob/ae9b586f34a8443573f82ef57c3bf914a908cbf7/packages/flutter/lib/src/widgets/scrollbar.dart>
- API：`PointerSignalResolver`、`ScrollPosition.pointerScroll`、`ScrollPhysics.shouldAcceptUserOffset`、`NestedScrollView`：<https://api.flutter.dev/flutter/gestures/PointerSignalResolver-class.html>、<https://api.flutter.dev/flutter/widgets/ScrollPosition/pointerScroll.html>、<https://api.flutter.dev/flutter/widgets/ScrollPhysics/shouldAcceptUserOffset.html>、<https://api.flutter.dev/flutter/widgets/NestedScrollView-class.html>
- Windows wheel 换算：`flutter_window.cc` L470/L799、`flutter_windows_view.cc` L647：<https://github.com/flutter/flutter/blob/ae9b586f34a8443573f82ef57c3bf914a908cbf7/engine/src/flutter/shell/platform/windows/flutter_window.cc>

## 附录 C：审查记录

> 审查方式：三名独立子审查者（架构/算法、仓库可行性/AGENTS 合规、证据/外部忠实度）对本文档与当前 HEAD 源码、调研备忘、探针/截图做只读复核。三份完整报告（本机 gitignored）：`E:\kytrix-scratch\research\review-{architecture,repo,evidence}.md`。以下为 findings 与处置摘要；全部 BLOCKER/HIGH 已在本文档回改。

### C.1 审查结论与处置

| # | 审查者 | finding | 严重度 | 处置 | 回改位置 |
|---|---|---|---|---|---|
| A-B1 | 架构 | `scroll_candidates` 代理环可无限循环（A 祖先→B 后代） | BLOCKER | **接受**：加 `visited`、`MAX_CHAIN_HOPS=32`、代理目标存活/滚动槽校验、`set_scroll_proxy` 拒绝后代目标；补 E10b 与变异项 | §4.3、§4.7、§7.1、§7.4 |
| A-H1 | 架构 | `Phase::End` 立即清会话导致 macOS momentum 重新仲裁 | HIGH | **接受**：End 只置 `ended_at`，Momentum 在 IDLE 内沿用；补 E27 | §4.2、§4.4、§7.1 |
| A-H2 | 架构 | 双轴 `ScrollbarAxis::point` 清零另一轴 | HIGH | **接受**：`point(current, offset)` 只改本轴；`scroll_state` 读当前值；补 K7b | §4.7、§5.2、§7.2、§7.4 |
| A-H3/R-H2 | 架构/仓库 | `A11yScroll.vertical: bool` 无法表达 Both | HIGH | **接受**：改 `orientation: Option<A11yOrientation>`；Both 不自动 | §4.6、§5.2、§5.5 |
| A-H4/R-M3 | 架构/仓库 | Input clear 在 `install_scroll` 时尚未创建，接线点错误 | HIGH | **接受**：改到 `set_clearable` 生命周期；补 K5/K10 | §4.7、§5.5、§6 P3 |
| A-H5/R-H4 | 架构/仓库 | `scroll_candidates` 未校验失效代理目标 | HIGH | **接受**：候选前校验 `scroll_slot(proxy).is_some()`，失效按无代理继续；补 E9b | §4.3、§7.1 |
| R-B1 | 仓库 | P1 文件作用域漏 `app/input.rs`，删 `ScrollAxis` 后不编译 | BLOCKER | **接受**：P1 加 `app/input.rs` 机械迁移；P2 再重写 | §5.5、§6 P1 |
| R-B2 | 仓库 | P0 红测引用尚不存在的 `set_scroll_proxy`/`ScrollAxes`，只能编译失败 | BLOCKER | **接受**：P0 只写 HEAD 可编译的行为红测；代理/双轴测试随 P1/P2 API 落地 | §6 P0 |
| R-H1 | 仓库 | Wayland 另有同名 `ScrollAxis` 枚举，不能迁移/全局 rg | HIGH | **接受**：明确排除，`rg 'interaction::ScrollAxis'` 限定 | §5.5、§6 P4 |
| R-H3 | 仓库 | `apply_to_target` 分轴调 `scroll_to` 会重置另一轴 | HIGH | **接受**：从原 offset 构造 goal，一次 `scroll_to` | §4.5、§5.2 |
| R-H5 | 仓库 | P2 验收要求 E17/E18，但键盘/a11y 统一在 P4 | HIGH | **接受**：E17/E18 移到 P4-1 验收 | §6 P2/P4 |
| R-H6 | 仓库 | `FieldAxis` 是布局判别器，不能替换为 `ScrollAxes` | HIGH | **接受**：保留 FieldAxis；只改 ScrollConfig/state.axes | §5.5 |
| R-H7 | 仓库 | `scroll_ancestor(s)` 仍有 text_edit/a11y 结构性调用 | HIGH | **接受**：保留为结构性助手，不强行删除；删除清单已更正 | §5.3、§5.5 |
| R-H8 | 仓库 | `set_scroll_proxy` 自指无合适错误变体 | HIGH | **接受**：新增 `EngineError::InvalidScrollProxy` | §5.1、§5.4 |
| R-H9 | 仓库 | `InputEvent::Scroll.timestamp` 被入口丢弃 | HIGH | **接受**：`dispatch_input_tracked` 透传到 `App::on_scroll`；补 E28 | §5.5、§6 P2、§7.1 |
| R-H10 | 仓库 | 零 delta `End/Cancel` 在早期 return 被跳过 | HIGH | **接受**：先处理 phase 再判零 delta；E8 覆盖 | §4.4、§6 P2 |
| R-M1 | 仓库 | `ScrollRouter` 包装/`window`/`started_at`/`ScrollSource` 冗余 | MEDIUM | **接受**：已删除结构体层与多余字段；`ScrollSession { target, kind, last_event_at, ended_at }`（`kind` 用于区分 Discrete/Continuous 的 End 语义） | §4.2、§5.1 |
| R-M2/M3 | 仓库 | Table header 无节点存储；Input clear 生命周期未覆盖 | MEDIUM | **接受**：`Geometry.header` + clear 生命周期 | §4.7、§5.2、§6 P3 |
| R-M4/M5 | 仓库 | gallery `state.axis` 遗漏；应用 crate 实际用 `ScrollConfig::default()` | MEDIUM | **接受**：§5.5 更正 | §5.5 |
| R-M6 | 仓库 | a11y 横向步长实际用 height | MEDIUM | **接受**：改 width；补 E32 | §4.6、§5.5、§7.1 |
| R-M7 | 仓库 | `--wheel-check` 与 `--idle-check` 时序未定义 | MEDIUM | **接受**：基线前完成并排空帧；分进程 | §6 P3、§10.1 |
| R-M8/M9 | 仓库 | `is_continuous`/`App::scroll_animation_target` 删除遗漏；doc 回写 §11 不存在、E22 多处 | MEDIUM | **接受**：删除清单与 §8.2 更正为 §8.4/E22 三处/E22.md 缺失 | §5.3、§8.2 |
| E-H1 | 证据 | §3.4 Qt Quick「序列内不换目标」错误 | HIGH | **接受**：改为“无跨事件目标锁，每事件重新 pointerTargets” | §3.4 |
| E-H2 | 证据 | §3.1 `:49-53`/`:67` 是 memo 行号；wheel_target 缓存条件缺失 | HIGH | **接受**：改 `input_handler.cc:2281` + `mouse_wheel_event_manager.cc:72-78` 条件 | §3.1 |
| E-H3 | 证据 | §2.2-A 与 C 实机证据混用（3 刻度同时有 88 页移） | HIGH | **接受**：A 只用 S1/S4；C 保留 88 证据 | §2.2 |
| E-H4 | 证据 | `ScrollAnimationTarget` 类型不存在；“唯一跨事件状态”过度断言 | HIGH | **接受**：改为 `AnimationTarget::Scroll`/`scroll_animation_target()`，限定“滚动目标状态” | §2.2 |
| E-M1~M6 | 证据 | `revalidate_hover` 调用点、Qt slider 行号、Flutter scrollbar 未验证、Qt Quick 表、`ScrollRouter` 残留、overscroll 表述 | MEDIUM | **接受**：逐条按报告更正 | §2.2、§3.2、§3.4、§3.5、§4.2 |
| E-L1~L5 | 证据 | Chrome main-thread scrollbar、Flutter caveat、L710→711、槽号措辞 | LOW | **接受**：表内加脚注、行号修正、措辞改为“全量重绘可能重排” | §3.1、§3.4、附录 B、§2.1 |

### C.2 复核状态

- 架构审查：Round 1 NOT-READY → Round 2 复核 B1、H1–H5 全部关闭，**verdict = READY**。
- 仓库审查：Round 1 READY-WITH-FIXES → Round 2 复核 B1/B2、H1–H10 全部关闭（H8/H10 补全后确认），**verdict = READY**。
- 证据审查：READY-WITH-FIXES → H1–H4/M1–M6/L1–L5 已按报告逐条更正（外部对比表、引用行号、A/C 证据分离、`AnimationTarget::Scroll` 措辞），无剩余 BLOCKER/HIGH。
- 剩余开放问题见 §9.2（O1–O10），需用户终审裁决；未裁决前不进入实施。复审原始报告追加在 `E:\kytrix-scratch\research\review-{architecture,repo,evidence}.md`。

## 附录 D：实施偏差登记（模板）

| 编号 | 阶段 | 计划 | 实际 | 原因 | 影响 | 证据 |
|---|---|---|---|---|---|---|
|  |  |  |  |  |  |  |

## 附录 E：实施补充入口

> 本附录由 `crates/ui_core/design/scroll-routing-addendum.md`（2026-11-26，下称“补充书”）引入；主计划书未被覆盖处仍以正文为准，编号不变。

| 事项 | 主计划书原状 | 补充书位置 | 覆盖的主计划书条款 |
|---|---|---|---|
| O1：Chrome 45° 规则与向量投影 | §3.5/§4.5 简化为“首个可消费 + 按启用轴整包消费” | 补充书 §1-A1、§2（源码事实、精确算法、边界、目标选择、逐候选/目标 `line_height` 换算、映射、E34–E38/E34m/E34n） | §3.1 对角 bullet、§3.5 不采纳第 3 条、§4.3 行单位、§4.5、§7.1 E12 |
| O3：Table 横向滚动 | 不在本批 | 补充书 §1-A2/§1.2-AD1–AD4、§3（宽度公式、`Column::flex` 三参数显式最小宽、span、Table 内部 sticky 表头、List 最小重构、水平/垂直条局部 rail 尾隙、a11y、迁移、K11–K21） | §4.6 “组件级横向滚动不在本批”、§9.1 R13、§9.2 O3、§5.2/§5.5 Table 条目 |
| gallery 宽表演示 | 无 | 补充书 §1-A3、§4（窄/宽视口对照、G7–G9） | §6 P3、§7.2/§7.3 |
| 增量任务与门禁 | P0–P4 既有任务 | 补充书 §5（P0-4、P1-5/6、P2-6/7、P3-6…P3-10、P4-6…P4-8）；**已同步写入主计划 §6 的 P1/P2/P3 任务表与文件作用域**（`:618-660` 附近） | §6 各阶段追加，不替换 |
| 风险/未验证/裁决 | §9 | 补充书 §6（R1–R9、U1–U3/U5、I1–I2、**AD1–AD4** 已裁决关闭） | §9.1/§9.2 增量 |

实施顺序、命令门禁（§10.1）、删除清单（§8.1）、不变量 I1–I9 与 D1–D6 裁决均不变；新增工作只出现在补充书 §5 列出的阶段增量。
