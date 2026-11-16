# 布局与交互审查

审查对象：`crates/ui_core`（HEAD `a9a4220`，只读）。范围：`layout/{taffy_tree,measure,hit_test}.rs`、`interaction/{focus,scroll,virtual_list,overlay,drag,animation}.rs`、`math.rs`、`event.rs`、`error.rs`，并交叉核对 `app.rs`/`node/tree.rs`/`paint/pass.rs` 中的消费者。

## 总体判断

布局内核的基本盘是好的：`taffy_tree.rs` 的低级 trait 适配基本是 taffy `TaffyView::compute_child_layout` 的逐段镜像——隐藏布局在 cache 之前短路、`PerformLayout` 才跑 `compute_oof_layout`、Block 与 FlowRoot 的 `block_ctx` 区分、只写 `set_unrounded_layout` 不做圆整；`measure.rs` 直接复用 taffy `Cache` 的 `(known, available, sizing_mode, run_mode, parent_size, axis)` 键，没有再维护第二份 `(measure_version, available)` 缓存，这个决策是对的。实测路径上**不存在每帧全量 taffy 重排**：`Window::layout_if_dirty` 只在存在 `LAYOUT` 脏位时调用布局，输入不变时 root cache 命中（`taffy_tree.rs:66-69`、`90-128`，测试 `second_run_without_changes_does_no_work`）。

问题集中在交互层和“布局后处理”的规模化能力：命中测试、焦点遍历、帧门禁都是线性全树扫描；可见性在 hit/paint/focus 三处的语义不一致；`Position::Fixed` 的布局结果与 paint/hit 的滚动偏移语义冲突；滚动偏移在内容/视口缩小时不重新夹紧；拖拽/浮层状态机的边界行为还停留在“能跑通”的阶段。对“复杂图表 + 大量节点 + 高频更新”的目标而言，当前更像是正确性优先的第一版，而不是已经做过性能压测的引擎。

分级结论：**P0 无；P1 4 条（可见性/焦点、Fixed、跨平台键码）；P2 7 条（滚动静默失效、O(N) 命中/焦点、滚轮链、拖拽点击、浮层边界、裁剪上限）；P3 1 条（颜色插值）**。详细见下。

## 发现（P0-P3）

### P1-1 焦点遍历不排除 `display:none` / `visible:false` 子树，Tab 会落到不可见节点

- 证据：`crates/ui_core/src/interaction/focus.rs:21-26`（`focusable` 只检查 `is_hittable`、`tab_stop`、action/explicit interactive）；`crates/ui_core/src/node/tree.rs:1522-1526`（`is_hittable` 只判断 `interactive != Some(false)`，不查 display/visible）；对照 `crates/ui_core/src/layout/hit_test.rs:109-115`（命中路径明确排除 `Display::None` 和 `visible == Some(false)`）。`app.rs:1315-1325` 的 `validate_focus` 只检查节点是否还存在，不检查是否可见。
- 问题：折叠面板、关闭的 dialog、`visible:false` 的幽灵节点里的带 action 节点仍进入 Tab 序列；节点在获得焦点后被隐藏，焦点也不会被清理。按 Enter/Space 仍会派发隐藏节点的 action。
- 影响：键盘用户焦点“消失”，可能误触发不可见业务动作；a11y 的焦点树与视觉树不一致，属于可感知的正确性缺陷，不只是性能问题。
- 成熟实现对照：Blink `Element::IsFocusable` 会检查 `display:none`、`visibility` 与 inert；Qt `QWidget::isVisibleTo`/`focusPolicy` 决定 focus chain；GTK `gtk_widget_get_visible` 与 focus chain 同步；Flutter 的 Focus 节点来自可命中的 render tree。
- 建议：抽出与 `hit_test` 共享的 `visible_for_interaction(tree, node)`（沿祖先链检查 `display != None` 且 `visible != Some(false)`），`focusable` 与 `hit_test` 共用；节点由可见变隐藏时在 `validate_focus` 中清焦点/迁移焦点。补一条“折叠面板内按钮不进入 Tab 序”的集成测试。
- 置信度：高。

### P1-2 `StylePatch.visible=false` 只隐藏自身图元，不隐藏子树；hit_test 却按整棵子树排除

- 证据：`crates/ui_core/src/paint/pass.rs:177-182` 只把 `display:none` 当作子树隐藏；`paint/pass.rs:438-451` 的 `visible` 判断只让 `PrimitiveEmitter` 跳过本节点自身图元，随后 `paint/pass.rs:245-247` 照常递归绘制子节点；而 `hit_test.rs:113-115` 在父节点 `visible == Some(false)` 时直接返回 `None`，整棵子树不可命中。
- 问题：`visible:false` 容器的子节点仍然被绘制，却全部不可点、不可聚焦（叠加上一条）。如果设计意图是“只隐藏自己”，则 hit_test 错；如果是“隐藏子树”，则 paint 错——无论哪种意图，当前实现都不自洽。
- 影响：出现“看得见但点不动”的死像素；浮层/面板切换时子内容残留；a11y 与视觉不一致。
- 成熟实现对照：CSS `visibility:hidden` 默认隐藏整个子树（子元素可显式覆盖）；Qt `QWidget::setVisible(false)`、GTK `gtk_widget_set_visible` 都是递归语义；Flutter `Visibility` 默认替代子树。
- 建议：明确 `visible` 是“自身”还是“子树”语义并统一三处；若为子树语义，在 `paint_node` 入口与 `display:none` 合并判断（并保留子节点覆盖的扩展位）；若为自身语义，hit_test 不应提前返回整棵子树。补“visible:false 父节点 + 子节点”的 paint/hit 双向测试。
- 置信度：高。

### P1-3 `Position::Fixed` 布局在视口，paint/hit 却按树祖先滚动偏移平移

- 证据：taffy 0.15 的 `Position::Fixed` 是 out-of-flow，`compute_root_layout` 会把 fixed 候选提升到 ICB（initial containing block）；引擎在 `taffy_tree.rs:124-126` 只对 `PerformLayout` 调 `compute_oof_layout`，`node/tree.rs:1835-1846` 的 hoisted 循环按包含块 origin 放置（fixed 最终落到 root origin）；但 `hit_test.rs:130-135` 和 `paint/pass.rs:239-246` 对**所有**树子节点无条件累加祖先 `scroll_slot.offset`。`Position::Sticky` 也没有 sticky pass（taffy 文档明确将 sticky 偏移交给调用方）。
- 问题：fixed 节点的 `layout().bounds` 是视口绝对坐标，但 paint/hit 又减去了它树路径上滚动容器的偏移 ⇒ 固定工具栏、固定图例、固定操作按钮在滚动面板里会跟着内容移动，命中区域也整体偏移。`Sticky` 则静默退化为 static。
- 影响：交易终端常见的“固定表头 / 固定操作栏 / 图表固定坐标轴”会出现视觉漂移和点击错位；这是布局与交互契约的直接冲突。
- 成熟实现对照：CSS `position:fixed` 的包含块是视口，祖先滚动不改变其绘制偏移；Blink 在 paint offset 中区分 fixed/scroll 图层；Qt 的 `Qt::WA_...`/窗口层级与 `QAbstractScrollArea` 分开；Flutter 通常用 `Overlay`/`Stack` 把 fixed 内容移出滚动子树。
- 建议：给节点维护 `scroll_exempt`（Fixed 后代到最近滚动祖先之间不累加 offset）或显式禁止在滚动子树内使用 Fixed；Sticky 要么实现滚动阈值 pass，要么在 API 文档/构建期拒绝。补“滚动容器内 fixed 子节点滚动后位置不变”的 hit+paint 测试。
- 置信度：高（静态推演；现有测试只覆盖 dirty 判定，无 fixed 布局/绘制用例）。

### P1-4 `Key::Other(u16)` 使用平台私有键码，应用级快捷键无法三平台通用

- 证据：`crates/ui_core/src/event.rs:57-61` 说明字母/数字的字符输入走 `Text`，快捷键身份用 `Key::Other`；`event.rs:103` 定义为 `Other(u16)`；`event.rs:283-289` 的 `KeyBinding` 明确用 `Key::Other(vk)` 表达字母/数字键位。平台映射各不相同：Windows `platform/event.rs:246` 直接保留 VK；macOS `platform/macos/view.rs:535-568` 把 `NSEvent.keyCode`（硬件码）塞进 `Other`；Linux `platform/linux/surface.rs:1306-1340` 把 evdev 码减 8 塞进 `Other`。
- 问题：应用写 `KeyBinding { key: Key::Other(0x53), modifiers: CONTROL, ... }`（Ctrl+S）只在 Windows 命中；macOS 上 0x53 不是 S 的 keyCode，Linux 上是完全不同的 evdev 值。三平台要么各写一份 `cfg` 表，要么快捷键静默失效。
- 影响：跨平台产品的全局快捷键（保存、切 symbol、图表缩放、下单确认）是核心交互；当前 API 使它们无法以同一身份表达，且失败是静默的。
- 成熟实现对照：Qt 用逻辑键 `Qt::Key_S`；GTK 用 `gdk::Key`；GPUI 用 `KeyCode`；Chrome 用 `ui::KeyboardCode`/DOM code；平台层负责把物理键归一化为逻辑键，`Other` 只应作为无法映射时的兜底。
- 建议：增加逻辑键枚举（至少 A-Z、Digit0-9、标点、F1-F24）或引入独立的 `KeyCode`；平台层做 physical→logical 映射，`Key::Other` 保留原始码但不再作为 `KeyBinding` 的正常路径；补三平台 Ctrl+S 同一 binding 的集成测试。
- 置信度：高。

### P2-1 内容尺寸缩小 / 视口变大后滚动偏移不重新夹紧，虚拟列表会空白

- 证据：`crates/ui_core/src/interaction/scroll.rs:22-40` 定义了 `max_offset`/`clamp_offset`，但唯一的调用点是 `app.rs:1521`（`scroll_to`）；`app.rs:1483-1497` 的 `set_scroll_content` 只写 `content`，不夹紧旧 offset；`app.rs:1718-1787` 的 `rewindow_virtual` 直接读取 `scroll.offset.y` 计算 `visible_range`，最后只更新 `slot.content`；窗口 resize 的 `window.rs:552-563` 只做 `invalidate_layout`，没有滚动归一化。
- 问题：用户滚到列表底部后把窗口拉高（或数据条数减少、`set_scroll_content` 调小），`offset` 仍大于新的 `max`；`visible_range` 的 `first` 被 `min(count)` 截到 count，返回空区间，现有行全部回收，屏幕空白。只要下一次滚轮/滚动调用才可能夹紧。
- 影响：窗口 resize、切换 symbol、刷新数据（条目变少）时列表空白/过卷；这是高频行情列表的常见操作路径。
- 成熟实现对照：Chrome `cc::ScrollableArea::ClampScrollOffset` 在 contents size 变化后统一夹紧；Qt `QAbstractScrollArea` 在 range 变化时夹紧 scrollbar；GTK `GtkScrolledWindow` 调整 adjustment；Flutter `ScrollPosition.applyContentDimensions` 会 clamp。
- 建议：布局/虚拟列表重开窗后统一执行 `normalize_scroll_offsets(window)`（对每个 scroll slot 按当前 viewport clamp，变化时写回并 damage）；`set_scroll_content` 和 `rewindow_virtual` 内也做一次；补“滚到底部后拉高窗口 / 把 count 减半”的回归测试。
- 置信度：高。

### P2-2 命中测试每次指针事件全树递归，且只对叶子做盒外剪枝，复杂图表下 O(N)

- 证据：`crates/ui_core/src/layout/hit_test.rs:129-158` 的子循环里 `let leaf = child_node.children().is_empty();`，只有**叶子**的盒外点会被 `continue`；所有非叶容器无论其 bounds 是否包含点都会继续 `pick` 递归（B2 为了避免漏掉溢出/出流后代）。`app.rs:1018`（PointerMove）、`1074`（PointerUp）、`1173`（Scroll）每次都调用 `hit_at`，即完整重跑。
- 问题：没有子树包围盒/溢出包围盒/空间索引。一个 10k 节点的图表，即使指针只在一个小区域移动，所有非叶分组都会被递归；叶子层还要逐节点做 `Rect::contains`。指针移动频率高时这是主要 CPU 热点之一。
- 影响：图表/表格 hover、拖动、滚轮的事件延迟；节点越多越明显，直接违背“大量高频更新、低延迟”的目标。
- 成熟实现对照：Blink `PaintLayer::HitTest` 先按 visual rect / hit-test rect 裁剪子树；Skia 用 `SkBBH`/R-tree 加速 picture hit；Qt `QWidget::childAt` 沿 widget 树的 allocation 裁剪；Flutter `RenderBox.hitTest` 先用 `size.contains(position)` 提前返回，`RenderSliver` 只测可见区间。
- 建议：在 `absolutize` 时维护每个节点的 `subtree_bounds`（union layout bounds + 溢出后代），剪枝条件改为“点不在 subtree_bounds 且该子树没有 Clip::Bounds 可逃逸”；或每帧构建扁平的可命中列表 + 空间索引。至少对 PointerMove 相同坐标做去重短路。补 10k 节点图表的 hit benchmark。
- 置信度：高（复杂度由代码结构确定，绝对耗时需实测）。

### P2-3 `focus_next`/`focus_prev` 每次 Tab 全树扫描并分配 `Vec`，且焦点不自动滚入视口

- 证据：`crates/ui_core/src/interaction/focus.rs:31-68` 的 `next_focusable` 每次调用都把 `tree.order()` 全量遍历、`filter(focusable)` 收集到 `Vec<NodeId>`；`app.rs:1423-1446` 每次 Tab/Shift+Tab 都调用。焦点变化路径没有 `scroll_into_view`；`a11y/action.rs:205-232` 已经实现了 `scroll_into_view`，但只服务 AT 的 `ScrollIntoView` action。
- 问题：每个 Tab 是 O(所有节点) + 一次堆分配；大表格/图表里按 Tab 会卡顿。即使焦点落在滚动视口外（尤其虚拟列表重开窗后焦点行还在但不可见），也没有自动滚动，用户看不到焦点。
- 影响：键盘导航性能与可发现性；对 a11y 和“无鼠标操作”场景是硬伤。
- 成熟实现对照：Qt `QWidget::nextInFocusChain` 用双向链表 O(1) 后继；GTK 维护 `GtkFocusChain` 列表；Chrome `FocusController` 的 sequential focus navigation 使用 scope 缓存；浏览器与 Flutter 在 focus 时调用 `scrollIntoView`/`Scrollable.ensureVisible`。
- 建议：维护按 `order`/`tab_stop` 生成的焦点链缓存，结构或 tab_stop 变更时增量失效；焦点改变后调用现有 `scroll_into_view` 并按需 `mark_paint`；退化为线性扫描时至少用迭代器 + `SmallVec` 避免每键分配。
- 置信度：高。

### P2-4 滚轮只选“最近滚动祖先”，不做滚动链；水平 delta 在非 Shift 情况下被丢弃

- 证据：`crates/ui_core/src/interaction/scroll.rs:53-62` 的 `scroll_ancestor` 只返回最近的带 `ScrollSlot` 节点，不检查该槽在请求方向上是否还能滚；`app.rs:1175-1193` 的 `on_scroll` 用 `drives = if Shift { delta_x } else { delta_y }`，没有 Shift 时 `delta_x` 完全忽略；`scroll.rs:90-95` 的 `wheel_target` 只按单轴和 `line_height` 计算。
- 问题：嵌套滚动（图表内横向时间轴嵌在纵向列表里、表格嵌在页面里）在边界处吞掉滚轮，无法传递给外层；横向滚动容器在触控板横向滑动（delta_x、无 Shift）下完全不动。
- 影响：嵌套行情表格“滚不动”的体感；横向时间轴必须按 Shift 才能滚，触控板用户尤其困惑。Chrome/Qt/GTK 的滚动链是用户预期行为。
- 成熟实现对照：Chrome `ScrollNode`/`ScrollChain` 逐级向上滚动并在边界传递；Qt `QAbstractScrollArea` 与 `QScroller` 的 propagation；GTK `GtkScrolledWindow` 的 kinetic scroll chaining；Flutter `Scrollable` 的 `ScrollPhysics`/parent notification。
- 建议：`scroll_ancestor` 返回候选链；按 `slot.axis`、当前 offset 和方向判断是否可滚，不可滚则继续向上；非 Shift 时若 `delta_y == 0` 且 `delta_x != 0` 使用 `delta_x`（或按最近滚动容器的轴选 delta）。补嵌套滚动边界测试。
- 置信度：高。

### P2-5 `on_drag` 在按下即开启会话，零位移也抑制 click；`drag_distance` 对拖拽节点不参与判定

- 证据：`app.rs:976-1008` 的 `on_pointer_down` 只要命中链上有 `on_drag` 就创建 `DragSession` 并派发 `DragPhase::Start`；`app.rs:1076-1102` 的 `on_pointer_up` 发现 session 就派发 `End` 并 `return`，永远不会进入 `1080` 行之后的 click 路径；`drag.rs:9-12` 的 `crosses_threshold` 只在非拖拽节点的 click 路径（`app.rs:1116-1118`）使用，`InputThresholds.drag_distance` 对 `on_drag` 节点没有作用。另：`on_pointer_up` 没有把 `session.current` 更新为抬起位置，`End` 的 delta 使用最后一次 Move 的位置。
- 问题：同一节点同时声明 `on_click` 与 `on_drag` 时，即使只是单击、位移为 0，也只会收到 Drag Start/End，Click 永不触发；拖拽启动没有阈值，Start handler 可能在每次普通点击时执行昂贵初始化；快速点击/最后一帧位移的 End delta 可能少一段。
- 影响：可拖拽 tab、图表平移、可拖拽行等无法兼具点击语义；分栏/图表拖动的结束位置可能偏差；触摸/触控笔点击体验差。
- 成熟实现对照：Qt `QApplication::startDragDistance` 后才创建 `QDrag`；Chrome `DragController` 的 drag threshold；GTK `gtk_drag_check_threshold`；Flutter `DragGestureRecognizer` 的 touch slop / long press。
- 建议：按下只记录 PressRecord；移动首次超过 `drag_distance` 才创建 DragSession 并派发 Start；未超阈值时按 click 路径派发；`PointerUp` 先把 `current` 更新为抬起坐标再算 delta。补 `on_click + on_drag` 的零位移/超阈值两条测试。
- 置信度：高。

### P2-6 浮层状态机边界：outside-click 越过顶层 `Explicit` 浮层；Above/Left 定位未计自身尺寸

- 证据：`crates/ui_core/src/interaction/overlay.rs:28-47` 的 `outside_click_target` 从 root 子节点逆序扫描，遇到 `dismiss != OutsideClick`（包括最顶层 `Explicit`）直接 `continue`，会继续找到下层 `OutsideClick` 并返回关闭；`interaction.rs:681-684` 的 `AnchorSide::Above` 只是 `bounds.origin.y - gap`、`Left` 只是 `bounds.origin.x - gap`，没有减去浮层自身宽高；`app.rs:243-257` 的 `overlay_style` 也只写 left/top，未设置 bottom/right。
- 问题：（a）打开一个 Explicit 模态浮层后，点击背景会关闭它下面的 OutsideClick 菜单，即使那个菜单被模态挡住——顶层优先语义被破坏；（b）Above/Left 定位只对齐锚点的左上角，浮层会覆盖锚点而不是放在其上方/左侧；同时没有 viewport 翻转/夹紧，靠近屏幕边缘的菜单会出界。
- 影响：嵌套 dialog/menu 的关闭顺序错误；右键菜单/下拉框在上方或左方时位置明显错误；交易图表边缘的上下文菜单不可用。
- 成熟实现对照：Chrome `MenuRunner` 只处理最顶层 bubble 的 outside-click；Qt popup/modal 的 window type 与 `QMenu::popup` 的屏幕适配；GTK `GtkPopover` 的 grab + 自动翻转；Chrome/GTK popup 都按实际尺寸做 flip/shift。
- 建议：outside-click 扫描遇到第一个 overlay 就停（若顶层不可 outside-close 则返回 None，并按模态策略消费该次按下）；定位结果改为“锚点 + 浮层实际尺寸”的求值（或在 overlay style 里使用 bottom/right inset），并加 viewport flip/shift。补 Explicit 叠加 OutsideClick、Above/Left 锚点、屏幕边缘三条测试。
- 置信度：高。

### P2-7 `MAX_CLIP_DEPTH = 8`：paint 直接失败整帧，hit 静默截断 clip_chain，两者行为不一致

- 证据：`crates/ui_core/src/layout/hit_test.rs:15-16` 复用 8 层上限；`hit_test.rs:53-59` 的 `ClipChain::push` 超过 8 层直接丢弃；测试 `hit_test.rs:345-364` 把“截断不 panic”固化为预期。paint 侧 `paint/pass.rs:228-236` 在 `clip.rects.len() >= MAX_CLIP_DEPTH` 时返回 `EngineError::ClipDepthExceeded`（`error.rs:104-118`），整帧 abort；`paint/scene.rs:18` 定义常量 8。
- 问题：9 层嵌套 `Clip::Bounds` 在 paint 是硬失败，在 hit 是静默截断；`HitResult.clip_chain` 的消费者（probe/应用）拿到的链不完整，且没有诊断。复杂图表/嵌套面板很容易出现“外壳滚动 + 面板 clip + 图表 clip + 行 clip + 浮层 clip”这类组合，8 层并不宽裕。
- 影响：合法复杂界面会因 clip 深度整帧失败（用户看到卡住/错误），而 hit 又可能给出错误裁剪信息；两个子系统对同一 UI 的极限不一致，难以定位。
- 成熟实现对照：Skia `SkClipStack` 会把矩形裁剪做交集而不是限制固定层数；Qt `QPainter::setClipRegion` 维护单一裁剪区域；Chrome `cc` 的 clip tree 和 Flutter 的 clip layer 都没有 8 层这种硬上限（Flutter 只受内存/栈实际限制）。
- 建议：对轴对齐矩形 clip 做累计交集（hit 只需要一个 Rect，paint 的前缀也可用交集 + 一个恢复栈），从根上消除深度上限；若保留上限，至少让 hit/paint 同阈值、同错误语义，并把 `ClipDepthExceeded` 作为可诊断错误而非整帧失败。补 10 层嵌套 clip 的 paint/hit 一致性测试。
- 置信度：高（行为由代码确定；真实 UI 触发频率需实测）。

### P3-1 `lerp_color` 自己实现非预乘 sRGB 插值，透明端点会发黑，且与 `ColorResolver::interpolate` 重复

- 证据：`crates/ui_core/src/interaction/animation.rs:41-69` 手工 `ColorResolver::resolve` → `to_srgb`（反预乘 + gamma 编码）→ 逐分量 `lerp` → `from_srgb`；`paint/color.rs:274-288` 已提供 `ColorResolver::interpolate`，明确在**预乘 alpha** 下按 `Interpolation`（默认 Oklab）插值并做 gamut mapping，`color.rs:615-624` 的 `interpolate_premultiplied_no_color_leak` 正是“透明端点不能漏色”的回归测试。
- 问题：`to_srgb` 对 alpha=0 的端点返回 `[0,0,0,0]`，再与实色插值会得到“黑→目标色”而不是“目标色以半透明出现”，透明→实色的过渡出现暗边/脏影；同时 `ColorResolver::new(Interpolation::default())` 的 `resolve` 根本不使用插值空间，实际插值空间是 sRGB，与引擎声明的默认 Oklab 策略不一致；每帧每端点还多做多次 `powf`。
- 影响：主题切换、hover/选中高亮从透明色过渡时有暗边/颜色突变；颜色动画 CPU 成本高于必要值；与 `paint` 层的颜色语义不一致。
- 成熟实现对照：CSS Color 4 与 Blink/Flutter/Skia 的颜色插值都在预乘空间进行（或按目标色彩空间做 gamut map），透明端点不会污染 RGB 分量。
- 建议：直接使用 `ColorResolver::interpolate(from, to, t)`（再把 `LinearPremulRgba` 转回非预乘 `Color` 供 `Transient<Color>`），或至少在预乘线性空间插值；补“透明红→不透明红”的中点颜色测试。
- 置信度：高。

## 做得好的地方

- **taffy 适配忠实度高**：`dispatch_child_layout` 的 hidden 短路、cache 包装、`Display::Block`/`FlowRoot` 的 `block_ctx` 差异、`compute_oof_layout` 只在 `PerformLayout` 执行，均与 taffy 0.15 `TaffyView` 对应；`run` 明确不调用 taffy 圆整入口，符合引擎“逻辑像素浮点、只在 paint 边界物理化”的 I11 约定（`taffy_tree.rs:50-129`）。
- **measure 缓存设计正确且克制**：不复制 taffy `Cache` 的键，`invalidate_measure` 清 node..root 缓存；对闭包原始返回值做 NaN/负值检测（`measure.rs:26-61`），错误定位到应用代码；测试覆盖 measure 缓存命中、失效、首条违规。
- **命中测试常见路径正确**：逆 DFS 优先、`hit_shape` 只用目标局部坐标、父 clip 只作用于子树、border band 仍可命中父自身、滚动偏移换算等都有明确测试（`hit_test.rs:248-677`）；`ClipChain` 是 Copy + 定长零分配。
- **数学层的物理像素语义拆得清楚**：`to_physical_f32`/`to_physical`/`to_physical_rounded`/`to_physical_covering` 四种用途、逐边取整避免相邻图元缝隙、NaN/∞/i32 饱和都有大表测试（`math.rs:233-321, 705-1079`），Window buffer 的 fractional scale 取整也考虑到了。
- **虚拟列表可见区间是纯函数**，边界（0 视口、overscan、尾部、行高 0）都有单测（`virtual_list.rs:14-76`）；滚动 clamp/max_offset 也是纯函数、可测。
- **错误面是强类型**：`EngineError` 无字符串通用包装、`Display` 只做诊断、i18n 留给应用层；`InvalidMeasure` 的 payload 足以定位节点/轴/值。

## 存疑 / 需实测

1. **`absolutize` 增量性的实际成本**：`node/tree.rs:1771-1774` 每次布局从 root `force=true` 开始，`node/tree.rs:1802` 对每个访问到的节点 `children.clone()`。即使 taffy 只重算脏子树，root 的 N 个子节点仍会被遍历/克隆；10k/100k 根级子节点场景需要 benchmark，确认“增量布局”在 post-pass 是否名不副实。
2. **每帧门禁的 O(N) 扫描**（跨模块，不在本次文件范围但影响交互延迟）：`window.rs:716-727` 的 `needs_frame` 与 `window.rs:744-752` 的 `layout_if_dirty` 都用 `any_node` 扫全树；`app.rs:2361-2371` 的 `refresh_styles` 每帧按 `order()` 全扫。滚动帧即使没有 bind 求值，仍是 O(N)，大节点树下需实测是否成为帧预算瓶颈。
3. **隐藏的定位容器 + 出流子节点可能留下 ghost box**：`node/tree.rs:1840-1846` 的 hoisted 循环无论当前节点是否 hidden 都以 `parent_hidden=false` 调用 `place_subtree`；而 `compute_hidden_layout` 不清理该节点的 `hoisted` 列表。`position:relative + display:none` 且含 absolute 子节点时，子节点可能保留非零 `layout.bounds`（N1 的反例）。建议构造该用例实测。
4. **`visible:false` 的设计意图**：`StylePatch.visible` 文档写“不绘制、不参与命中”，但未说是否递归；当前 hit/paint 不一致。确认意图后按 P1-2 统一。
5. **`Position::Sticky` 的支持边界**：taffy 明确把 sticky 偏移交给调用方，引擎没有 sticky pass；是明确不支持并在 API 拒绝，还是后续实现，需要设计决策。
6. **动画起始时钟**：`app.rs:1591`（Scroll）与 `app.rs:2017`（Scalar/Color）用 `self.clock` 作为 `start`，而 `sync_clock` 由平台泵每轮调用；若 `animate` 从异步任务/非分发路径调用，`start` 可能落后于真实时间，首帧会跳进度。需要核对平台泵的 `sync_clock` 与 `UserTask` 执行顺序。
7. **8 层 clip 上限的真实触发率**：当前测试只验证“10 层不 panic”，没有验证 paint 的错误恢复；需要在真实复杂图表布局（滚动 + 面板 + 行 + 浮层）上统计最大 clip 深度。
