# 核心状态与帧调度审查

## 总体判断

`ui_core` 在状态建模上方向正确：依赖图按 `(窗口, 节点代际)` 区分 owner、写入是 push 模型、arena 带代际、帧提交点集中，这些都具备演进成高频 GUI 引擎的基础。但当前实现仍以“每帧全树扫描 + 全场景重编码”为主，细粒度依赖只节省了样式闭包的求值，并未节省布局、文本、绘制与实例编码的固定成本；再加上节点卸载不清理依赖、文本形状缓存仅 256 条、虚拟列表存在布局时序缺口，整体尚不适合 Kytrix 这种大量文本 + 高频更新 + 多窗口的场景。多窗口调度目前是全局唤醒位 + 串行全窗口帧，缺少按窗口的唤醒与预算控制，一个窗口的重绘会波及其它窗口。若目标替代 GPUI 支撑行情/复杂图表，应先解决 P1 中的依赖生命周期、文本缓存、虚拟列表正确性和帧级增量四类问题，再谈着色器与 GPU 后端优化。

## 发现（按 P0/P1/P2/P3 排序）

### [P1] 虚拟列表在 layout 之后 rewindow 挂载的行永远不会被布局（0×0 直到滚动）

- 证据：`crates/ui_core/src/app.rs:2655`（`layout_if_dirty`）→ `crates/ui_core/src/app.rs:2679`（`rewindow_virtuals` 在布局与 `layout_text` 之后才执行）；`crates/ui_core/src/app.rs:1754-1783`（`rewindow_virtual` 在布局后 `build` 新行并 `set_virtual_row`）；`crates/ui_core/src/window.rs:1131-1139`（`settle_frame` 无条件清 `LAYOUT | PAINT` 并置 `REUSABLE`）；`crates/ui_core/src/app.rs:1788-1791`（`changed` 时只 `mark_structure` + `request_frame`）。
- 问题：首次建窗/`make_virtual_list`/`set_virtual_items` 常在布局前发生，此时 `scroll::viewport` 为 0，`visible_range` 只挂 1 行；首帧 `layout_if_dirty` 只布局这一行。随后 `App::frame` 在布局之后才调 `rewindow_virtuals`，根据新视口挂载其余行——这些节点插入于 layout 之后，本帧从未参与 taffy 布局。帧尾 `settle_frame` 对全树无条件清掉它们的 `LAYOUT` 并置 `REUSABLE`，下一帧 `needs_frame` 为 false、`layout_if_dirty` 也看不到 `LAYOUT`，于是这些行永久保持 0×0，直到滚动或其它无关布局失效。`rewindow_virtual` 里的 `request_frame()` 也救不回来，因为下一帧没有 `LAYOUT`。
- 影响：虚拟列表首屏只显示 1 行（或 overscan 行），其余可见行空白；窗口 resize 改变可见行数时同样出现缺失行。Kytrix 的行情表/成交列表正是核心场景，属于直接可见的正确性缺陷。
- 成熟实现对照：Flutter 的 sliver 在 layout 阶段按 viewport 惰性构建子项并当场完成布局；Qt `QAbstractItemView` 在 `updateGeometries` 中先算可见区再对新建 delegate 项布局；Chrome LayoutNG 的虚拟化列表也保证新挂载的 LayoutObject 在同一布局代内参与布局，不会留下“未布局的已挂载节点”。
- 建议：在 `settle_frame` 中只清 `laid_out_epoch == layout_epoch` 的节点的 `LAYOUT`；未布局的新节点保留 `LAYOUT` 到下一帧。或者把 `rewindow_virtuals` 拆成两阶段：布局前按上一帧 viewport 预挂载，布局后再 rewindow 并在 `changed` 时同一帧再跑一次 `compute_layout`（sliver 式后布局）。补一条测试：容器未预布局时首帧后断言行 `bounds.height == extent`。
- 置信度：高

### [P1] 文本 pass 每帧对所有文本节点重新 shape 查询，`ShapeCache` 仅 256 条导致超过 256 个不同文本时每帧全量重排

- 证据：`crates/ui_core/src/app.rs:2831-2866`（`refresh_text` 无条件遍历 `tree.order()`，对每个文本节点 `text_content(node).cloned()`、`text.shape_cached(&content, scale)`、再 `ShapeKey::new`）；`crates/ui_core/src/text/cache.rs:66`（`ENTRY_LIMIT: usize = 256`）与 `crates/ui_core/src/text/cache.rs:81-88`（每次命中还做 `order.iter().position` 线性扫描）；`crates/ui_core/src/text/context.rs:89-116`（`shape_cached` 在查到 node 级缓存前就构造 key、miss 即重新 shape）；`crates/ui_core/src/app.rs:2844-2853`（只有 `Rc::ptr_eq` 命中才 skip）。
- 问题：`refresh_text` 没有文本脏集，任何一帧只要进入就会遍历全部文本节点。每个节点先走全局 `ShapeCache`（无命中则 Parley 重新 shape），再构造一次 `ShapeKey`（对整段文本做第二次哈希）。当窗口内不同 `TextContent` 超过 256 个时，LRU 在两次全量扫描之间必然发生逐出：每个节点每次都 miss、每次返回新的 `Rc<ShapedText>`，`Rc::ptr_eq` 永远为 false；于是 `set_text_shaped` 清空该节点的 `line/broken` 并 `invalidate_caches_upwards`，随后 `layout_text` 又对所有文本节点重排、重新光栅。大表格里 1000+ 个不同单元格会变成每帧全量 reshape + 全量断行。
- 影响：Kytrix 的行情表/深度图/成交列表通常远超 256 个不同字符串；一个窗口即可把主线程钉在 Parley shape 上，60Hz 完全不可达。即使不超过 256 条，每帧对所有文本做两次全文哈希和 O(256) LRU 扫描也是纯浪费。
- 成熟实现对照：Chrome LayoutNG 把 shaping 结果缓存在 `ShapeResult` 并跟随 LayoutObject 生命周期，只有文本/样式变化才重排；Skia `ParagraphCache` 可按工作集配置容量且不强制逐出仍在使用的条目；GPUI/egui 把文本布局作为 element 缓存，只在内容或尺寸变化时重建。
- 建议：给 `NodeTree` 增加 `text_dirty` 计数/集合，`refresh_styles` 改写文本时只把该节点加入；`refresh_text` 只处理集合内节点。先把 node 的 `TextSlot.key/scale_epoch/Rc` 检查放到全局缓存查询之前（缓存键可直接存进 `TextSlot`）。`ShapeCache` 改为弱引用或按窗口工作集自适应容量，避免节点已持有 `Rc` 时仍被全局 LRU 逐出。
- 置信度：高

### [P1] `FrameScheduler` 没有 RAII abort：样式/文本/绘制 panic 后窗口永久卡在 `Draining`，后续帧全部 `Idle`

- 证据：`crates/ui_core/src/window.rs:187-194`（`FrameTicket(())` 无 `Drop`）；`crates/ui_core/src/window.rs:215-239`（`begin` 置 `Draining`，只有显式 `commit`/`abort` 复位）；`crates/ui_core/src/window.rs:730-741`（`begin_frame` 在非 `Idle` 时直接返回 `None`）；`crates/ui_core/src/app.rs:2633-2635`（`begin_frame` 返回 `None` 被当成 `Idle` 返回）。
- 问题：`App::frame` 在 `begin_frame` 之后依次调用 `refresh_styles`、`refresh_text`、`layout_if_dirty`、`paint_frame`，全部是用户闭包可 panic 的路径。panic 展开时 `FrameTicket` 只是被 drop，`FrameScheduler.state` 永远停在 `Draining`；下一次 `App::frame` 的 `needs_frame` 仍为 true（dirty 位还在），但 `begin_frame` 因 `request_frame()` 在非 `Idle` 返回 false 而返回 `None`，`App::frame` 返回 `Ok(Idle)`。窗口从此不再提交任何帧，且没有任何 API 能把它复位。`App::run_frame_hook`、`dispatch_action`、`a11y_take_handler`、`rewindow_virtual` 的 builder 也有同样的 take-call-put 模式（见 P2）。
- 影响：任何一个 `bind_style` / `bind_text` / `bind_a11y` / measure / 绘制闭包写出 panic（测试里已有 `bind_style_panic_clears_ghost_deps`、派生 panic 用例），在宿主用 `catch_unwind` 包住 `App::frame` 的嵌入/测试场景下窗口会静默冻结；引擎文档宣称的“panic 后继续服务”不成立。生产平台虽然会让 panic 终止进程，但这也意味着一次 UI 闭包 panic 直接杀掉整个交易终端。
- 成熟实现对照：wgpu 的 `SurfaceTexture`/frame guard、GPUI 的 frame guard 都在 `Drop` 中回滚到 Idle；Chromium 在 `BeginFrame` 失败时显式 `AbortFrame`；Qt 的 `QPainter::begin/end` 采用 RAII，异常路径也必须 `end()`。
- 建议：把 `FrameTicket` 改成带 `Drop` 的 guard：默认 `abort()`，`commit()` 用 `mem::forget`/`ManuallyDrop` 消费；在 `App::frame` 外层加 `catch_unwind`，把 panic 转成 `EngineError::AppInit`/新错误类型并保持窗口可继续运行；补 `catch_unwind(App::frame)` 后再次 `App::frame` 的回归测试。
- 置信度：高

### [P1] 节点卸载不解除依赖边，陈旧 owner 在依赖图中无界累积，写路径越来越慢

- 证据：`crates/ui_core/src/node/dependency.rs:35-38`（注释明确“已卸载的 owner 允许残留，由 `App::write` 的 retain 惰性剔除”）；`crates/ui_core/src/app.rs:1745-1749`（虚拟列表回收行直接 `tree_mut().remove(*row)`）；`crates/ui_core/src/app.rs:1890-1898`（`remove_node` 只 `tree_mut().remove` + `request_frame`）；`crates/ui_core/src/node/tree.rs:656-719`（`NodeTree::remove` 只清旁表，不接触 `DependencyGraph`）；`crates/ui_core/src/app.rs:2162-2186`（`close_window` 只退订当前仍存活的节点，之前已 remove 的节点已不在 `order`）。
- 问题：依赖图只有 `replace_deps`（重新求值时差分）和 `close_window` 对存活节点的 `detach`。`App::remove_node` / 虚拟列表回收 / 应用直接 `tree_mut().remove` 都不会删除 `Owner::Node` 边；只有下一次写同一个信号时 `invalidate` 里的 `owners.retain` 才做一次惰性清理。对共享的行情信号（一个 symbol/价格流被大量行读取），每回收一行就留下一个死 owner；滚动越久，下一次 `write` 的 `retain` + 快照复制成本越高，图内存也单调增长。`NodeId` 的代际能让死 owner 被识别，但不会主动回收。
- 影响：长时间运行的行情终端中，虚拟列表滚动/浮层开关会持续累积死依赖；`write` 是高频热路径，最终从 O(活跃行) 退化为 O(历史挂载过的所有行)，并伴随每次写克隆整个 `OwnerSet`（`app.rs:3043`）的堆分配。这是“细粒度依赖”在真实高频场景下最容易失效的地方。
- 成熟实现对照：GPUI 的 `SubscriberSet` 在 entity/Element drop 时同步移除订阅；Qt 的 `QObject` 析构自动断开所有 signal-slot 连接；Chrome 的 PaintInvalidation 集合随 LayoutObject 销毁从失效列表中移除，不做“下次写时扫描”。
- 建议：让 `NodeTree::remove` 返回被删子树的 `(NodeId, SmallVec<SignalId>)`，由 `App::remove_node` / `rewindow_virtual` 在删除前 `graph.detach`；或让 `DependencyGraph` 用 `(WindowId, slot index)` 索引并在节点槽位释放时按槽清理。至少给虚拟列表回收加批量 detach，并周期性清扫 `windows.get(window).is_none()` 的 owner。
- 置信度：高

### [P1] 每个提交帧有约 8-10 趟全树扫描，且每次提交全量重编码整个场景并跨线程复制

- 证据：`crates/ui_core/src/app.rs:2361-2489`（`refresh_styles` 遍历 `tree.order()`，末尾 `any_node` 两次）；`crates/ui_core/src/app.rs:2484-2485`（`layout_dirty`/`paint_dirty` 两个全树扫描）；`crates/ui_core/src/app.rs:2722-2725`（生产构建 `let _ = (&styles, &text, &text_layout, &layout)`，这两个报告字段根本没有消费者）；`crates/ui_core/src/app.rs:2831-2867`（`refresh_text` 全树）；`crates/ui_core/src/app.rs:2902-2942`（`layout_text` 全树）；`crates/ui_core/src/app.rs:1702-1714`（`rewindow_virtuals` 全树收集）；`crates/ui_core/src/window.rs:716-727`（`needs_frame` 全树）；`crates/ui_core/src/window.rs:744-752`（`layout_if_dirty` 再全树）；`crates/ui_core/src/window.rs:977-982`（`needs_node_repaint` 再全树）；`crates/ui_core/src/window.rs:1032`（`self.encoder.encode(&self.scene, ...)` 每次提交编码全部图元）；`crates/ui_core/src/window.rs:1174-1177` + `crates/ui_core/src/render/frame.rs:72-88`（`FramePayload::from_buffer` 复制全部实例字节/ops/clips/geometry）；`crates/ui_core/src/node/tree.rs:1131-1140`（`settle_frame` 全树）。
- 问题：一次只改一个节点的颜色，`App::frame` 仍会做：`needs_frame` 全树扫描 → `refresh_styles` 全树 + 2 次统计扫描 → `refresh_text` 全树（还带文本 shape 查询）→ `layout_if_dirty` 全树 → `mark_moved_a11y` 全树 → `layout_text` 全树 → `rewindow_virtuals` 全树 → `paint_frame` 全树 DFS + 全场景 `InstanceEncoder::encode` → `settle_frame` 全树。最后还把整个实例缓冲复制到 GPU 线程。`damage` 只用于 GPU 呈现裁剪，完全不减少 CPU 侧编码成本。
- 影响：以 100k 节点、几十万图元的复杂图表为例，单节点更新就要上百万次节点访问和几十 MB 实例编码/复制；60Hz 下每帧预算被固定成本吃光，行情高频更新时会出现主线程饱和、输入延迟。细粒度依赖追踪在这里只优化了“哪些闭包要求值”，没有优化“帧要为全树付多少钱”。
- 成熟实现对照：Chrome 用 LayoutObject 的 dirty 集合和 paint invalidation set，只遍历失效子树；Qt 用 `update()` 区域和 widget 级重绘；GPUI 的 element tree 通过 `is_dirty`/subtree dirty 跳过干净分支，且仅重建变化的 element；Skia 保留 display list/GPU buffer，只重录 damage 区域。
- 建议：为每种工作维护节点脏集合（layout/paint/text/a11y），帧初取走集合、按集合处理；`needs_frame` 改为 O(1) 计数器；`paint_frame` 增加“子树无脏”聚合位，整棵干净子树直接跳过；`InstanceEncoder` 改为按 lane 持久化实例区间、只重编码脏 lane 并做范围上传；`StylePassReport` 的统计移入 `#[cfg(feature="test-support")]`。
- 置信度：高

### [P1] `NodeTree::order_insert` / `subtree_len` / `order_pos` 使建树、插入、删除为二次复杂度

- 证据：`crates/ui_core/src/node/tree.rs:1967-1970`（`order_pos` 线性扫描整个 `order`）；`crates/ui_core/src/node/tree.rs:1973-1984`（`subtree_len` 每次调用分配 `SmallVec` 并遍历整棵子树）；`crates/ui_core/src/node/tree.rs:2002-2022`（`order_insert` 对**每个已有兄弟**调用 `subtree_len`，再对子树每个节点做一次 `Vec::insert`）；`crates/ui_core/src/node/tree.rs:2025-2031`（`order_remove_subtree` 同样先 `order_pos` 再 `drain`）；`crates/ui_core/src/app.rs:1762-1782`（虚拟列表逐行 `build` 循环）。
- 问题：`NodeBuilder::build` 每次只建一个节点。向一个已有 k 个兄弟的父节点追加第 k+1 个节点时，`order_insert` 会遍历全部 k 个兄弟并各自走完整子树求和；同时 `Vec::insert` 还要移动尾部元素。总建树成本是 O(n²)（若子树深则为 O(n²·子树规模)）。`remove` 每次都 `order_pos` 全表扫描。虚拟列表一次滚动进入十几行就会触发多次这种全表/全兄弟扫描；首次构建几万节点的静态图表会明显卡顿。
- 影响：Kytrix 的复杂图表/多 pane 布局在启动或切换 symbol 时重建大量节点，会出现秒级构建；滚动虚拟列表出现周期性掉帧。这个成本与高频数据无关但决定首屏和交互手感。
- 成熟实现对照：GPUI/Flutter 使用 arena + 子节点链表/连续子数组，append 为 O(1) 摊销；绘制顺序由一次 DFS 在帧初生成，不在每次插入时维护全局先序数组；Qt `QObject::children` 追加也是 O(1) 摊销。Chrome 的 LayoutObject 树同样不维护全局 splice 数组。
- 建议：在节点上缓存 `subtree_len`（插入/删除时沿祖先更新），`order_insert` 用一次 `Vec::splice` 插入整块；或标记 `order_dirty`，在帧初用一次 O(n) DFS 重建 `order`，把结构操作从 O(n²) 降到 O(n)；提供批量 `build_subtree` API 供虚拟列表一次挂多行。
- 置信度：高

### [P1] 滚动动画每帧经 `scroll_to` 调 `request_frame` 自唤醒，绕过平台 60Hz 节流形成忙循环

- 证据：`crates/ui_core/src/app.rs:2102-2106`（`tick_animations` 的 `AnimationTarget::Scroll` 分支调用 `self.scroll_to(handle, point)`）；`crates/ui_core/src/app.rs:1506-1536`（`scroll_to` 在末尾 `self.request_frame()`）；`crates/ui_core/src/app.rs:744-747`（`request_frame` 置 `wake_requested` 并 `dispatcher.wake()`）；`crates/ui_core/src/platform/macos/event_loop.rs:205-208`（注释明确警告“连续帧不能反过来自己 `dispatcher.wake()`，否则 source 会在同一轮 run loop 里反复唤醒，主线程变成忙循环（实测 830fps）”）。
- 问题：`tick_animations` 在 `App::frame` 内运行，平台本身已经用 60Hz 定时器/display link 调度连续帧；但滚动动画的 `scroll_to` 每帧都调用 `request_frame`，重新置位 `dispatcher` 的电平信号并唤醒 run loop。macOS 的唤醒 source 会立即再次进入 `pump`，不必等 60Hz 定时器；Windows 则是 `SetTimer(1ms)` + `WM_APP` 立即唤醒，同样绕过节流。滚动动画因此变成“帧内自唤醒”循环，直到动画结束。
- 影响：鼠标滚轮平滑滚动是行情/图表最核心交互之一；在 macOS 会跑出数百到上千帧/秒、CPU 单核打满并饿死输入；Windows 上也会造成 1ms 定时器叠加消息唤醒的高 CPU。多窗口下每个滚动动画还会连带其它窗口的 `frame_all`。
- 成熟实现对照：Qt 的动画 tick 只 `update()`（合并到下一次事件循环，不额外唤醒）；Chrome 的动画由 BeginFrame 驱动，帧内不允许再请求 BeginFrame；GPUI 的 `App::notify` 合并到当前帧，frame 结束后由平台按 vsync 调度。
- 建议：拆出 `scroll_to_with_wake(handle, offset, WakePolicy)`：`tick_animations` 里用 `WakePolicy::None`（只改 offset + damage，依赖平台连续帧），用户输入路径才唤醒；或在 `App::frame` 内设置 `in_frame` 标志，`request_frame` 只置位不 `dispatcher.wake`，帧尾统一决定是否需要下一帧。补测试：一个滚动动画运行 N 帧只应产生 N 次平台定时器帧，不应产生自唤醒。
- 置信度：高

### [P2] 多窗口共享一个全局 `wake_requested`，由第一个被 `frame` 的窗口消费，且 `frame_all` 无预算/公平性

- 证据：`crates/ui_core/src/app.rs:731-747`（`App` 只有单个 `wake_requested: bool`，`take_wake_request` 全局清零）；`crates/ui_core/src/app.rs:2621-2628`（每个 `App::frame(window)` 开头都 `let wake = self.take_wake_request();`）；`crates/ui_core/src/platform/windows/event_loop.rs:958-967` 与 `crates/ui_core/src/platform/macos/event_loop.rs:290-299`（`frame_all` 从 `app.windows()` 收集 `Vec`，按 `ahash::HashMap` 任意顺序逐个 `app.frame(id)`）。
- 问题：一次 `dispatcher.wake()` 只对应一个全局布尔。`frame_all` 中第一个窗口的 `App::frame` 把它消费掉并获得 `needs_frame(wake=true)`，即使该窗口没有任何脏节点也会跑完整样式/文本/布局/绘制；真正需要该唤醒的窗口只能依赖自己的 dirty 位。纯唤醒（`AppHandle::wake` 用于外部内容更新、无 dirty 的情况）会被第一个窗口静默吞掉，目标窗口永远不出帧。串行 `frame_all` 也没有时间预算：一个重绘窗口会阻塞后面所有窗口，HashMap 顺序还会让饿死窗口不固定。
- 影响：多窗口行情工作区里，一个图表更新会让另一个无关图表走一遍完整帧；高频更新时多个窗口互相拖慢，输入事件排队；无法把“唤醒”定向给某个窗口。
- 成熟实现对照：Chrome 每个 RenderWidget 有独立的 `needs_begin_frame`/invalidation，合成器按 frame sink 调度；Qt 每个 top-level `QWindow` 独立 `update()` 与 expose 事件；Wayland/AppKit 也天然按 surface 调度。成熟实现不会用进程级布尔代表所有窗口的帧需求。
- 建议：把 `wake_requested` 改为 `ahash::HashMap<WindowId, bool>` 或每 `Window` 内的 `frame_requested`；`invalidate` 知道被标脏的窗口，应只置对应窗口位；`frame_all` 只对 `needs_frame` 为 true 的窗口出帧，并按时间片（如每窗口 8ms 上限）轮转，保证公平。
- 置信度：高

### [P2] `capture_request` / `cursor_pending` 未携带 WindowId，Windows 泵对第一个窗口 `SetCapture`，第二个窗口拖拽捕获错窗口

- 证据：`crates/ui_core/src/app.rs:134-137`（`cursor_pending: Option<Cursor>`、`capture_request: Option<bool>` 都是全局单值）；`crates/ui_core/src/app.rs:990-992`（拖拽起始 `self.capture_request = Some(true)`）；`crates/ui_core/src/app.rs:1082-1083`、`crates/ui_core/src/app.rs:1937-1938`（抬起/取消置 `Some(false)`）；`crates/ui_core/src/platform/windows/event_loop.rs:721-735`（`if let Some(native) = windows.first_mut()` 然后 `SetCapture(hwnd)`/`ReleaseCapture()`）。
- 问题：`App` 层没有记录“哪个窗口请求捕获”。Windows 泵把捕获应用到 `windows.first_mut()` 的 HWND，而不是发生指针按下的窗口。用户在第二个窗口拖动分栏/图表时，`SetCapture` 会捕获第一个窗口，后续鼠标消息投递到错误窗口，拖拽会话收不到 Move/Up，表现为拖拽卡死、误点其它窗口或抬起丢失。`cursor_pending` 同理是全局最后一个值，多窗口同时 hover 时目标不确定（`SetCursor` 是线程级，问题稍小但状态不可分）。
- 影响：多窗口交易终端的拖拽（分栏、图表平移、窗口拖动）在非首个窗口不可用；这是多窗口隔离的直接正确性缺陷。
- 成熟实现对照：Win32 的 `SetCapture` 本来就是 per-HWND 状态；Qt `QWidget::grabMouse()` 作用于控件所属窗口；Chrome aura 的 `WindowTreeHost` 每个原生窗口独立管理 capture。成熟实现不会把 capture 请求放在进程级全局。
- 建议：改为 `capture_request: Option<(WindowId, bool)>`，Windows 泵按 id 找到对应 `NativeWindow` 再 `SetCapture`；`cursor_pending` 改为 `Option<(WindowId, Cursor)>`，Linux/macOS 平台按窗口忽略或应用。补两窗口拖拽集成测试。
- 置信度：高

### [P2] 写信号只按依赖标脏，不做 `StylePatch`/文本输出 diff，输出完全相同的节点仍清 `REUSABLE` 重建 lane

- 证据：`crates/ui_core/src/app.rs:2373-2379`（每个待求值节点先 `old_patch.clone()`）；`crates/ui_core/src/app.rs:2403-2435`（计算新 patch 后只比较 `affects_layout` 与 `interactive/visible`，从不做 `patch == old_patch`）；`crates/ui_core/src/app.rs:2436-2479`（文本只有 `changed` 用于标位，节点在 `invalidate` 阶段已清 `REUSABLE`）；`crates/ui_core/src/app.rs:3051-3070`（`invalidate` 对每个读者无条件 `mark_paint`/`mark_layout`）；`crates/ui_core/src/node/tree.rs:1877-1892`（`touch` 清 `REUSABLE` 并累积 damage）。
- 问题：一次 `write` 会沿依赖图把所有读者标脏，`refresh_styles` 虽然算出了新 `StylePatch` 和文本，却不拿它们与旧值做完整比较。即使 `bind_style` 的条件分支导致输出 patch 与上一次完全相等、`bind_text` 输出同一字符串，`REUSABLE` 已在 `invalidate` 时被清掉，paint pass 仍会重建 lane、产生 damage、编码图元。派生值级联和共享信号（主题、可见性、格式化后文本）会频繁产生这种“无效重算”。
- 影响：高频行情信号如果被多个节点读取，而节点最终样式/文本没变，仍然按全量读者数重建 lane 并重绘，GPU/CPU 都被浪费；这会放大 P1 中“全场景重编码”的成本。
- 成熟实现对照：GPUI/egui 在元素样式 diff 后只对 changed 字段标脏；Qt 的 `QStyle`/property 系统只在属性值真正变化时 `update()`；Chrome 的 paint invalidation 会比较新旧 visual rect/属性，相同则不加入 invalidation set。
- 建议：求值结束后比较 `patch == old_patch`、文本是否变化、a11y 是否变化；若全部不变，则清 `PAINT`、恢复 `REUSABLE` 并清掉本次 `touch` 累积的 damage（或在 `invalidate` 阶段延迟到求值后再清）。对文本节点单独走“text_changed”短路，避免同字符串仍重建 lane。
- 置信度：高

### [P2] `set_scale`/`set_size` 无同值短路；`set_scale` 每次递增 epoch 并清空进程级字形图集，所有窗口文本一起重栅格

- 证据：`crates/ui_core/src/window.rs:552-563`（`set_size` 不比较旧值，直接 `full_damage_pending = true` + 根 `invalidate_layout`）；`crates/ui_core/src/window.rs:594-606`（`set_scale` 不比较旧值，`scale_epoch = wrapping_add(1)` + 根失效）；`crates/ui_core/src/app.rs:2813-2817`（任一窗口 `scale_epoch` 与 `scale_epochs` 记录不一致就 `atlas.clear(); glyph_atlas.clear();`，两个对象都是进程级单例）；`crates/ui_core/src/app.rs:513-567`（`upload_glyph_page` 遍历所有窗口，缺页窗口整页 `queue_atlas_upload`）；`crates/ui_core/src/render/windows/renderer.rs:651-667`（每个窗口的 `queue_atlas` 还会 `pixels.to_vec()` 复制整页）。
- 问题：`set_scale` 是公开 API，平台层每次 DPI 事件都会调用；同值调用也会推进 `scale_epoch`。epoch 变化使该窗口下一次 `refresh_text` 清空**全局**字形图集和字形缓存，所有窗口的 tile 全部失效，每个窗口都要重新 shape/光栅/整页上传。`set_size` 同值也会触发全窗口重排和全量 damage。多显示器不同 DPI 或拖动窗口跨屏是交易终端的常见操作，一次 DPI 变化会造成所有窗口文本重栅格卡顿。
- 影响：多窗口文本密集界面在 DPI 切换/窗口跨屏时出现数百毫秒级卡顿；同值 `set_scale`/`set_size` 被应用层误调用时也会触发同样的全量失效。图片图集整页（可达 16 MiB）向每个窗口复制/上传，也放大广播成本。
- 成熟实现对照：Chrome/Skia 按 device scale 维护字形缓存或至少按 ppem 分桶，某个窗口 scale 变化不会清空其它窗口的 glyph cache；Qt 的 `QFontEngine` 按 screen/scale 缓存；GPUI 的 text system 以 `(font, scale)` 缓存并引用计数，不会全局 purge。
- 建议：`set_scale`/`set_size` 先比较旧值，同值直接返回；`set_scale` 只在真正变化时递增 epoch。把字形图集按 scale/ppem 分代或按窗口分组（至少不要 `atlas.clear()` 全局清空，改为标记该 scale 的 tile 失效）；整页上传只发给实际使用该图集的窗口。
- 置信度：高

### [P2] `set_virtual_items` 的 count/builder 变化不请求帧，`set_scroll_content` 不 clamp 偏移：列表缩短时可能整表空白

- 证据：`crates/ui_core/src/app.rs:1645-1663`（`set_virtual_items` 写 `slot.count`/`slot.builder` 后调用 `rewindow_virtual`）；`crates/ui_core/src/app.rs:1784-1791`（只有 `changed`（挂载/回收行）才 `mark_structure` + `request_frame`）；`crates/ui_core/src/app.rs:1718-1731`（`rewindow_virtual` 用当前 `scroll.offset` 计算 `visible_range`，不 clamp）；`crates/ui_core/src/interaction/virtual_list.rs:28-34`（offset 超出 count 时可能算出 `first..first` 的空区间）；`crates/ui_core/src/app.rs:1483-1498`（`set_scroll_content` 只写 `slot.content`，既不 clamp 也不 `request_frame`）。
- 问题：当 count 从 100 变成 90、而可见区间仍是 0..4 时，`rewindow_virtual` 只更新 `slot.content`，`changed=false`，不标结构也不唤醒——滚动条/内容高度不会重绘。若列表缩短到当前 offset 之外（例如从 1000 行缩到 10 行，offset 仍在 5000），`visible_range` 返回空区间，`rewindow_virtual` 回收所有行且不挂新行，列表变成空白，直到用户手动滚动（且 `scroll_to` 才 clamp）。`set_scroll_content` 对普通滚动容器同样会让 offset 悬空。若这些调用来自 `AppHandle::send` 的异步任务（帧门禁之后），还会一直等到下一条无关消息才出帧。
- 影响：行情列表/成交明细在数据源缩短、切 symbol、过滤后条数变化时出现空白或滚动条不更新；这是数据驱动 UI 的常见路径。
- 成熟实现对照：Qt 的 model `layoutChanged`/`dataChanged` 会触发 view 重新计算 scroll range 并 clamp；Android RecyclerView 的 `notifyItemRangeRemoved` 会触发 requestLayout，布局时 clamp scroll；Chrome 的 list virtualization 在 range 变化时立即请求 paint invalidation。
- 建议：`set_virtual_items` 无条件 `request_frame`（即使 visible range 不变，因为 content 尺寸变了）；在 `rewindow_virtual` 开头先把 `scroll.offset` clamp 到 `max_offset(slot, viewport)`；`set_scroll_content` 写入后 clamp 并 `request_frame`。补“在底部缩短列表”和“异步改 count”用例。
- 置信度：高

## 做得好的地方

- `Owner` 同时带 `WindowId` 与带代际的 `NodeId`，跨窗口依赖不会串号；`ReadTracker` 去重、`replace_deps` 做差分替换、`EvalGuard` 的 `Drop` 保证派生闭包 panic 时求值栈仍能出栈（`crates/ui_core/src/node/dependency.rs:21-29`、`108-134`、`174-197`）。
- `Arena` 的 `NodeId` 使用 `NonZeroU32` 代际 + free list，槽位复用时会清空 `layout_cache/text_slots/debug_ids/clip_keys/a11y_slots`，避免 ABA 与旧布局继承（`crates/ui_core/src/node/id.rs:100-193`、`crates/ui_core/src/node/tree.rs:784-812`）。
- `FrameScheduler` 把 `FrameIndex` 的唯一递增点收口到 `commit`，`Idle` 不推进序号，`frame_reaches_gpu` 二次门禁避免发布不会产生 GPU 工作的帧（`crates/ui_core/src/window.rs:190-244`、`1060-1107`）。
- `close_window` 会遍历存活节点精确退订依赖；`settle_frame` 作为唯一帧结算点统一复位 damage/`prev_layout`/`REUSABLE`，状态集中（`crates/ui_core/src/app.rs:2162-2186`、`crates/ui_core/src/node/tree.rs:1125-1140`）。
- `App::write` 的等值短路 + `StateStore::set_owned/set_transient` 复用已有 `Box`（避免每帧堆分配），`Dispatcher` 的 `AtomicBool` 合并唤醒 + Windows 线程消息兜底也符合高频消息泵的需求（`crates/ui_core/src/app.rs:715-723`、`crates/ui_core/src/node/signal.rs:392-428`、`crates/ui_core/src/platform/dispatcher.rs:111-179`）。
- `bubble_layout` 在父节点固定尺寸/出流时提前停止，避免无意义地一路冒泡到根；脏位设计有清晰的 `LAYOUT/PAINT/ORDER/A11Y/REUSABLE` 分层（`crates/ui_core/src/node/tree.rs:1894-1923`、`crates/ui_core/src/node/dirty.rs:39-66`）。

## 存疑 / 需实测

- 文本形状缓存临界点：机制上 >256 个不同 `TextContent` 会开始逐出，但实际每帧卡顿取决于文本节点数量、Parley shape 成本与 `layout_text` 重排比例；建议用 1k/5k 单元格基准量化。
- 虚拟列表“布局后挂载行永远 0×0”需要按真实初始化顺序（不预先对容器跑一帧）做 snapshot/`Probe` 验证；现有 `virtual_app` 测试先 `frame` 再 `make_virtual_list`，掩盖了这个路径。
- `FrameScheduler` panic 卡死可在 `catch_unwind` 包裹 `App::frame` 后连续调用两次验证（当前没有该测试）；生产平台不捕获 panic，但嵌入/测试场景需要明确行为。
- 全场景 `InstanceEncoder::encode` + `FramePayload` 复制的实际成本需要以 Geometry 节点为主的图表测量，确认是否真的是主线程瓶颈（若大部分图元走 GPU geometry，实例数量可能较小）。
