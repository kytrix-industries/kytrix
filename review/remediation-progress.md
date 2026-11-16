# ui_core 修复进度台账（2026-11-16）

基线：`a9a4220`（审查报告 `ui_core-review-2026-11-16.md` 所针对的提交）
分支：`ui-core-p0-p2-remediation`

每个条目 = 一个已提交、三平台验证过的改动。验收门禁：`cargo test -p ui_core --features test-support`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo fmt --check`、`cargo doc --no-deps`。

## P0（2/2 完成）

| 编号 | 问题 | 修复要点 | 提交 |
|---|---|---|---|
| P0-1.1 | 实例基址错位 | `render/uniform.rs` 成为 `RootUniform` 唯一构造点；硬件基址恒 0、实例偏移只经 `draw.clip.y` 进 shader（三 API 的硬件语义不同，见 HLSL 提案 0015）；`build.rs` 加锚点防回退 | `7c85b3b` |
| P0-1.2 | 几何通道未闭环 | 新增 `GeometryStore`（注册/释放/代际）；`GeometryDraw` 增加 `color`；三平台按需创建几何缓冲 + 几何 PSO（Linux/macOS 原先完全没有） | `7c85b3b` |

## P1（11/13 完成）

| 编号 | 问题 | 修复要点 | 提交 |
|---|---|---|---|
| P1-2.1 | 局部刷新不产生局部成本 | `FrameOp` 带物理包围盒 + damage 裁剪；damage 合并到 `MAX_DRAW_PASSES`；`ResolveCache` 按位键缓存颜色解析。实测 `frame_incremental_512` 288→72 µs，`frame_full_512` 222→142 µs | `73014f9` |
| P1-2.2 | 虚拟列表首屏 0×0 | `settle_frame` 只清本轮真正参与布局的节点的 `LAYOUT`（`laid_out_epoch`） | `949e980` |
| P1-2.3 | 文本热路径 | 文本脏集合驱动；`font_features`/`tnum`；`approx_bytes` 改按字形计；形状缓存 256→1024 条 / 16 MiB。实测 `text_shape_32` 3.23→1.75 ms | `7b19883`、`adb44e2` |
| P1-2.4 | `FrameTicket` 无 RAII | 加 `Drop`（默认 abort），状态改 `Rc<Cell<_>>` 以便 panic 展开时复位 | `949e980` |
| P1-2.5 | 卸载节点不解依赖边 | `NodeTree::remove` 返回被删子树的 deps，`App` 同步 detach | `949e980` |
| P1-2.6 | 图集换代不清其他窗口文本 | `mark_all_text_paint` + `atlas_generation_seen`，换代时让所有窗口文本重绘（只标 PAINT） | `85753c1` |
| P1-2.7 | 图片图集由首图锁定 | 图片页固定最大档，容纳能力与插入顺序无关 | `1dcce75` |
| P1-2.8 | 窗口生命周期静态 / Linux 单窗口 | Windows/macOS 运行时窗口创建与销毁（`73a194a`）；Linux：Stage A 提取 `WindowSurface`（`e29cf5c`）、Stage B 窗口集合化 + 事件按对象路由（`315d107`）、Stage C-1 scale 协商下移（`ef3fa34`）、Stage C-2a 事件带 `WindowId` + 焦点/指针分离路由（`0708dd7`）。**剩余 Stage C-2b**：`add_window`/`remove_window` + 每窗口渲染器 | 见左 |
| P1-2.9 | 每窗口独立 GPU 设备 | **未开始** | — |
| P1-2.10 | 剪贴板越界读 | `GlobalSize` 限定扫描范围，明确奇数长度/空块/无 NUL 的契约 | `949e980` |
| P1-2.11 | Linux 输入四处硬伤 | IME 双通路抑制；客户端合成键盘重复（`repeat_info` + 到期合成 + dispatch timeout 压制）；`axis_value120`/`frame` 批处理与正确量纲；`modifiers.group` 传入 `update_mask` | `cc94e65` |
| P1-2.12 | macOS 节奏与上传同步 | `CAFrameRateRange` 三点钉住刷新率；动画定时器按屏幕刷新率；上传槽改 per-slot semaphore（原一把 3-permit 与乱序完成回调不一致，会重写 GPU 在读的缓冲） | `c933fd1` |
| P1-2.13 | Windows 全链 `INFINITE` | fence 2 s / present 1 s×4 / 关闭 10 s 后 detach，超时按 `DeviceLost` 上报；GPU 线程空闲等待保留 `INFINITE` 并注明理由 | `398643c` |

## P2（15 条已修）

| 问题 | 修复要点 | 提交 |
|---|---|---|
| 顶点 clip 夹紧破坏图集 UV | UV 按未夹紧四边形求比例；三套 shader + contract §5 | `c67c8a9` |
| 布局冒泡污染祖先 damage | 祖先只标 `LAYOUT`，不再把自己的盒子并入 damage（价格文本变化不再整帧重绘） | `148b675` |
| 滚动偏移不重新夹紧 | `App::frame` 开头每帧重夹（`set_scroll_content` 不触发重排） | `c0ffb1c` |
| `Clip::Bounds` 不裁剪自身内容 | `emit_content` 用自身 padding box 包 Push/Pop；深度超限返回 `ClipDepthExceeded` | `c0ffb1c` |
| 焦点遍历不排除隐藏子树 | 可见性谓词收敛到 `NodeTree::is_visible_locally` / `is_effectively_visible` | `434e43d` |
| `visible: false` 语义不一致 | paint 与命中测试/焦点/a11y 统一为「隐藏整棵子树」 | `434e43d` |
| `Plan::merge` 只拼接不合并 | 拼接后按 `MAX_DRAW_PASSES` 合并 | `434e43d` |
| Linux 设备选择不排除 CPU / 不优先独显 | 按 `DEVICE_TYPE` 打分（独显 3 > 集显 2 > 虚拟 1 > CPU 0；CPU 保留作无头兜底） | `10959eb` |
| Linux swapchain 不用 min/maxImageExtent、usage、compositeAlpha | 三者全部校验/夹紧，缺能力时报 `UnsupportedGpu` | `10959eb` |
| Linux 离屏 `submit_snapshot` 不清 `fresh` | 同步完成后 `fresh = false` + `presented()` | `10959eb` |
| Windows 适配器选择只取第一个非软件 | `EnumAdapterByGpuPreference(HIGH_PERFORMANCE)`，`IDXGIFactory6` 不可用时回退 | `183157a` |
| macOS 图层未设 `colorspace` | 显式 sRGB，双屏颜色一致 | `183157a` |
| `Node` 尺寸预算 | `TextStyleExtras`（Blink `RareInheritedData` 模式）把 family + font_features 收进一次间接，`Node` 1152→1128 B | `990fd1d` |
| 失效 rustdoc 链接 | `cargo doc` 纳入三平台门禁 | `53c69f9` |
| 远端验证基建静默失配 | `tools/remote/rsync.ps1` 改为全量同步 + sha256 两端比对（旧实现用 `git diff` 只报未提交改动，提交后同步变成 no-op） | `73014f9` |

## 剩余

1. **P1-2.8 Stage C-2b**：`WaylandState::add_window`/`remove_window` + 每窗口渲染器 + 事件循环按窗口枚举。
2. **P1-2.9**：`Device`/`Queue`/`Pipeline`/`AtlasTextures` 提升到进程级，每窗口只留 swapchain/persist/fence（三后端）。
3. **P2 约 35 条**：见报告 §3.1–§3.4。
4. **对抗式审查**：全部 P0/P1/P2 逐条复核（本台账是审查输入）。

## 验收状态（本轮最后一次全量运行）

| 平台 | 测试 | clippy | fmt | doc |
|---|---|---|---|---|
| Windows（本机） | 847 + 6 + 4 + 8 + 36 全绿 | 干净 | 干净 | 干净 |
| Linux（192.168.130.128） | 776 + 6 + 4 + 8 + 36 全绿 | 干净 | — | 干净 |
| macOS（192.168.1.6，nightly-2026-11-09） | 769 + 6 + 4 + 8 + 36 全绿 | 干净 | — | 干净 |
## 对抗式自查发现（待修）

### D-1【P0-1.2 遗留，真实显存泄漏】几何 GPU 缓冲永不释放

- **现象**：`GeometryStore::release` 在 `src/` 下**没有任何生产调用点**（只有自身单测）。
  应用即使按契约调用 `app.geometry_mut().release(id)`，也只是从 CPU 注册表移除条目；
  三后端的 `GeometryRegistry.entries`（`render/{windows,linux,macos}/…`）按
  `(index, generation)` 缓存 GPU 缓冲，**只在代际不匹配时重建，从不清除**。
  于是「每次数据更新注册一份新几何、释放旧的」这种用法（价格图每 tick 重画折线就是）
  会让显存无界增长。
- **影响平台**：Windows（`compositor.rs` 的 `GeometryRegistry`）、Linux
  （`render/linux/geometry.rs`）、macOS（`render/macos/geometry.rs`）全部。
- **修法（Chrome 风格：按使用帧数驱逐）**：`GeometryRegistry` 每项记录
  `last_used_frame`，`ensure` 时更新；每帧末调用 `retain_recent(now, EVICT_AFTER_FRAMES)`
  驱逐超过 N 帧（建议 180）未使用的项，被驱逐的设备对象交给现有 retire 队列。
  这样既不泄漏，也不会让「隔帧绘制」的几何反复重建。
- **为什么不能直接按 `GeometryStore` 存活判断**：渲染器在 GPU 线程只拿帧快照，
  没有 store 的实时视图；用「最近使用」是自洽且不需要反向通道的做法。
- **验收**：三后端各加单测（不建真实设备的那部分：记录/驱逐决策可脱离 GPU 测），
  demo 加一个「每帧重建几何」的压力场景验证 `peak` 不增长。

### D-2【并发限制，已知】`App::frame` 的 `wake_requested` 是进程级

Windows / macOS 的事件循环每轮对**所有**窗口调 `App::frame`，且 GPU 完成回调会重新
`dispatcher.wake()`，因此被门禁挡住的窗口不会永久饿死（Linux 侧 subagent 已用
`deferred_frames` 额外兜底）。结论：**当前实现正确，但依赖「门禁重开会重新唤醒」这一
不变量**；若将来引入不唤醒的新路径，需要改成每窗口唤醒位。已记录，不单独修。

## 本轮自查修复（已提交）

- `bcffbac`：op 包围盒按 `params0.w`（shader 的顶点外扩）计算 —— 旧实现只外扩 1 px，
  会把阴影模糊环排除在包围盒外，damage 裁剪时整条阴影消失（可见缺陷）。
- `897d872`：空裁剪容器不再虚占 clip 深度 —— 抽出 `content_needs_clip` 让深度校验与
  `emit_content` 共用同一判据。
- `5e5d37c`：Linux Stage C-2b（subagent 完成，已独立复核）：原生多窗口 + 每窗口渲染器 +
  `WindowSurface::drop` 显式析构协议对象（wayland-client 代理 drop 不发析构请求）。

## P1-2.9 进展（本轮）

- **Windows 完成**（`83a8599`，subagent 完成、已独立复核）：进程级 `SharedGpu` 持有
  `ID3D12Device` / 一条命令队列 / 根签名 + 4 个 PSO / 共享 SRV 堆 / 两份图集纹理；
  每窗口只留 swapchain、DirectComposition 对象、persist 内容、提交流（allocator/list/
  fence/InFlightRing/retire）、实例上传缓冲、几何缓冲、RTV 堆与 SRV 堆窗口段。
  图集并发方案：每窗口独立 staging + 共享 GPU 纹理，页换代是进程级串行事件（写锁 + 有界
  fence 等待）。
  顺手修了两处既有泄漏：几何换代退役句柄收集后从未 retire；每窗口常驻资源关窗时不注销。
  测试 849 → **858**（+9）。
- **Linux / macOS 未做**：仍是每窗口独立 device/queue/pipeline/atlas。两后端需要同样处理
  （Linux 的 `VkDevice`/VkQueue/VkPipeline、macOS 的 `MTLDevice`/PSO/纹理）。

### 新增待修项

**D-3【上传带宽重复】共享图集纹理后，`app.rs` 仍逐窗口广播图集页** —— 显存已共享，
但同一份页内容会按窗口数重复上传。真正的「一页只推一次」需要把 `App` 的页分发改成
进程级单例 + 页版本。属 `app.rs`（root 写集），下轮处理。

**D-4【测试构建有两个进程级设备】**：产品硬件路径 `process()` 与 test-support 的
`process_warp()` 各持一个。发布物只有硬件路径，无害但要知道。

## D-1 状态：Windows/macOS 已修（`ff96957`）

`GeometryRegistry` 增加 `last_used_frame` + registry 单调帧号：
`begin_frame()`（每内容帧录制前）→ `ensure` 记录使用帧 → 帧末 `retain_recent()` 驱逐
`当前帧 - last_used > EVICT_AFTER_FRAMES(=180)` 的条目，设备对象进现有 retire 队列。
判据抽成纯函数 `geometry_is_stale(frame, last_used)`（`wrapping_sub` 容回绕），可脱离 GPU 测。
测试：Windows 859（+1）、macOS 772（+1）。**Linux 侧由并行任务在同一文件内实现**。

## P0-1.1 对抗式核对结论（通过）

三套顶点着色器逐字一致地把硬件基址恒传 0、只经 `root.draw.clip.y` 加偏移：

- `quad.hlsl:16-17`：`uint base = (uint)draw.clip.y; instances[instance_id + base]`
- `quad.vert.glsl:13-15`：`uint base = uint(root.draw.clip.y); instances[uint(gl_InstanceIndex) + base]`
- `quad.metal:19`：`uint base = uint(root.draw.clip.y);`

三个后端的所有 draw 调用硬件基址均为 0：
`DrawIndexedInstanced(…, 0, 0, 0)` / `cmd_draw_indexed(…, 0, 0, 0)` /
`drawPrimitives_…_baseInstance(…, 0usize)`。`build.rs` 的 `INSTANCE_BASE_ANCHORS`
在编译期强制这三行存在，任何回退会直接构建失败。

## 本轮（Round 16）P2 进展

| 项 | 修复 | 平台 |
|---|---|---|
| Windows P2-6 / macOS P2-4 失焦修饰键粘住 | Windows `TranslateState::reset_transient()` + WndProc 失焦分支；macOS `GuiView::focus_changed(false)` 清位图 | 双平台 |
| Windows P2-1 `WM_GETMINMAXINFO` 语义 | 客户区最小尺寸按 `dpi::window_size_for_client` 换算成窗口外框 | Windows |
| Windows P2-3 `WM_SETCURSOR` 未接管 | 记住引擎光标；`HTCLIENT` 时重新 `SetCursor` 并返回 1，非客户区交回系统 | Windows |

### 新增待办

**D-5【Windows P2-5】应用层捕获丢失无取消路径**：`WM_CAPTURECHANGED` 只处理自绘边框缩放；
应用自己 `SetCapture` 的拖拽在 Alt+Tab / 弹出系统菜单后丢失捕获，永远收不到 `PointerUp`，
拖拽会话卡死。修法：新增 `InputEvent::PointerCancel`，贯穿 `event.rs` → `App::dispatch` →
`interaction`（结束拖拽/悬停/按压态），Windows `WM_CAPTURECHANGED`（非缩放分支）与
macOS `resignKey`、Linux 指针 `leave` 都投递它。**需要跨三层，单列一轮。**

**D-6【macOS P2-6/P2-7】** 已派给并行 agent：去掉每帧整面 persist→drawable blit；
`WindowRenderer::drop` 的 join 改有界等待 + 超时 detach。

## Round 17：D-5 已修（`98e2db4`）

新增 `InputEvent::PointerCancel` / `PlatformEvent::PointerCancel`；
`App::cancel_pointer_session(window)` 从 `on_window_focus_lost` 抽出，失焦与捕获丢失共用
同一套收尾；Windows `WM_CAPTURECHANGED`（非自绘缩放）投递取消。
回归测试 `pointer_cancel_ends_the_drag_without_committing_a_click`。

三平台：Windows 861 / Linux 792 / macOS 782。

### 最新待办清单（P2 剩约 22 条）

**交互层（跨平台，风险中）**
- layout-interaction P2-2：命中测试每次指针事件全树递归（复杂图表 O(N)）→ 子树包围盒缓存
- layout-interaction P2-3：`focus_next/prev` 每次 Tab 全树扫描 + 分配 Vec；焦点不自动滚入视口
- layout-interaction P2-4：滚轮只选最近滚动祖先，不做滚动链；非 Shift 时水平 delta 被丢弃
- layout-interaction P2-6：outside-click 越过顶层 `Explicit` 浮层；Above/Left 定位未计自身尺寸
- layout-interaction P2-7：`MAX_CLIP_DEPTH` paint 整帧失败 vs hit 静默截断，行为不一致

**Windows 平台**
- P2-2：`WM_DPICHANGED` 先按 (100,100) 重排再套系统矩形（跳变 + 重复布局）
- P2-4：多窗口路径把指针捕获/光标/连续帧定时器硬编码到 `windows.first_mut()`，首窗关闭后定时器不迁移
- P2-7：IME 始终开启，无非文本输入禁用路径
- P2-8：`WM_CLOSE` 无条件关窗，`CloseRequested` 只观察不可拦截；`WM_QUERYENDSESSION` 未处理
- P2-9：标题栏拖动/系统菜单仍进 `DefWindowProcW` 模态循环，泵阻塞

**macOS**：P2-3 display link 屏幕参数只在建窗时读；P2-5 IME 生命周期未绑 first responder；
P2-8 未构建应用菜单

**文本/资源/a11y**：text-res P2-2 整 ppem + hint + 无亚像素定位；P2-4 `TextPassReport` 统计未填充；
P2-6 a11y live region 与节流；P2-7 表格行列语义；P2-8 控件默认动作；P2-9 选区 TextRun 语义

**Linux**：P2-2 `VK_ERROR_SURFACE_LOST_KHR` 未走恢复；P2-6 swapchain 销毁未证明 presentation engine 已释放图像；
P2-7 上传缓冲每帧 map/unmap（应持久映射）

**paint 共享**：P2-3 `Canvas` bounds 由应用手工维护，错误即静默 under-damage

## Round 33 状态快照（53 commits，三平台绿）

### 已完成（P0 2/2、P1 13/13、P2 44 条）

P2 按组汇总（每条均已提交并三平台验证）：

- **渲染/性能**：damage 裁剪 + 合并、op 包围盒按 shader 外扩、布局冒泡不污染祖先 damage、命中测试子树包围盒剪枝、
  Tab 焦点零分配、滚轮 scroll chaining、clip 超深折叠为求交、几何缓冲按最近使用驱逐（D-1 显存泄漏）、
  Linux 上传缓冲持久映射、macOS 局部 blit（drawable 差距追踪）、Windows/Linux/macOS 单设备多窗口
- **正确性**：UV 按未夹紧四边形、`Clip::Bounds` 裁自身内容、滚动偏移重夹、`Position::Fixed` 语义、
  可见性/焦点/命中/a11y 四处统一、`PointerCancel`（拖拽卡死）、浮层 outside-click 遮挡与 Above/Left 定位、
  三平台失焦复位修饰键、Windows 最小尺寸/DPICHANGED/光标接管/多窗口定时器迁移、`on_close` 可拦截
- **平台**：Linux 设备打分与 swapchain 能力校验、Linux 多窗口（Stage A/B/C-1/C-2a/C-2b）、
  Windows 优先高性能适配器、macOS 图层 sRGB、macOS join 有界化、窗口生命周期动态化
- **文本/资源/a11y**：形状缓存预算按字形计、字体特性/tnum、图集页容量与顺序解耦、a11y 行列语义/
  数值通道/live region/默认动作/限流到 10 Hz、文本光栅统计接线

### 剩余 7 条（均为跨 3–4 文件改动，各需完整一轮）

| 编号 | 内容 | 风险 |
|---|---|---|
| a11y-1 | 选区 `TextRun` / `character_lengths` 语义 | 中（需选区数据通道） |
| win-1 | IME 禁用路径（`ImmAssociateContext(NULL)`）+ `App` 开关 API | 中（跨 app/platform） |
| win-2 | 标题栏拖动/系统菜单的 `DefWindowProcW` 模态循环阻塞泵 | 高（替代方案需自绘非客户区） |
| mac-1 | display link 屏幕参数换屏后不更新（需 `Renderer::set_frame_rate` 四层plumbing） | 中 |
| mac-2 | IME 生命周期未绑 first responder | 中 |
| lin-1 | `VK_ERROR_SURFACE_LOST_KHR` 恢复路径 | 中（需重建 surface/swapchain） |
| txt-1 | 亚像素定位（atlas key ×4、hint 策略） | 高（需真机目视验证） |

### 收尾计划

1. 剩余 7 条逐条实现并三平台验证（预计 4–6 轮）
2. 更新本台账为最终版（逐条对照报告 §3.1–§3.4）
3. 派 3 个 subagent 独立复审（渲染栈 / 平台+交互 / 文本+a11y+资源），要求每条结论附证据与复现步骤
4. 汇总验收报告：三平台门禁数据、逐项对照、未完成项与理由

## 复审 P2 逐条处置（Round 48 更新）

- **P2-4（Windows 共享 SRV 堆无锁）**：已核实、**非缺陷**。D3D12 Resource Binding 功能规范
  "Creating Descriptors" 明确写 "All methods for creating descriptors are free threaded"；
  各窗口写不相交区间 ⇒ 并发合法。Linux 侧必须加锁是 `vkUpdateDescriptorSets` 的外部同步
  要求。已在 `render/windows/pipeline.rs` 模块文档写明依据与两平台差异原因。
- **P2-5（根 border 下浮层不收敛）**：已核实、**非缺陷**（Round 47 用测试否证：本仓库 taffy
  的 `inset` 相对 border box 原点、不扣 border）。保留回归测试防止误改。

## 新发现：P0 级缺陷（Round 59，非原报告条目）

**静态几何/Path 从未真正上传**：`InstanceEncoder` 各持一个空的 `GeometryStore`，App 的
注册表从未接入任何编码器（全仓无 store 赋值点）。`geometry_desc()` 恒走 MISSING 空描述
⇒ index_count = 0 ⇒ `Primitive::Geometry` 与 `Primitive::Path` 永远不产生像素。
现有测试只断言 op 顺序与 id，未断言顶点字节到达编码器，所以一直没被发现。

已修（commit 83089e3）：`encode(..., &GeometryStore)` 参数化、`paint_frame` 接收几何表、
App 用字段级借用拆分传入、独立窗口路径传进程级空表；新增回归测试断言真实顶点字节到达编码器。

它是复审 P2-2（`GeometryDesc.bounds` 死字段）的根因：desc 根本没走到消费端。
P2-2 本身（bounds 未参与 damage/命中）仍未处理。

## Round 66 快照（2026-11-16/17）

- 累计提交：**86**（基线 a9a4220，分支 ui-core-p0-p2-remediation）。
- 三平台门禁：Windows 893 / Linux 822 / macOS 812 lib 测试（+54 集成/示例），
  clippy `-D warnings` / fmt / doc 全干净。
- 第二轮对抗式复审（3 份报告）报出的 P1/P2/P3 **全部处理完毕**：
  - P1：macOS 几何 VBO 覆盖、op 包围盒测试空转、P0 修复缺 App 级回归、Windows 捕获
    suppress 修一半（F1/F2/F9/A2/A4）；
  - P2：a11y pos_in_set 语义（0-based + size 放容器）、滚动动画目标未夹紧、NaN 公开入口
    永动、Click 能力忽略祖先委托、Linux held_keys 三条漏清、macOS 关窗双投递（F3/F5）；
  - P3：几何 damage 隐藏/卸载/移动升级、request_close 返回值、is_hidden 策略、图集文档
    口径、控制字符清零（全库 0 处）、set_size 同值短路、统计双计（F4 浮层 margin 风险
    已记录为已知设计假设，未改）。
- 新增：`review/FINAL-ACCEPTANCE.md`（验收报告）。
- 待决 5 条原始 P2：a11y TextRun、Windows IME 禁用、macOS IME replacementRange
  （可代码内解决）；win-2 模态循环、txt-1 亚像素（需用户决策 / 真机目视）。

## Round 75 最终快照（2026-11-17）

- 累计提交：**91**（基线 a9a4220，分支 ui-core-p0-p2-remediation）。
- 剩余 4 条原始 P2 全部完成（commit f863426）：win-1 IME 禁用、win-2 自绘标题栏交互
  （用户选 B：消除 Win32 模态循环，保留系统标题栏绘制）、mac-2 IME replacementRange、
  txt-1 亚像素定位（用户选 A）。
- 第三轮对抗式复审（只读 subagent）审这 4 条修复：无新 P0，报出 1 条真 P1
  （macOS NSNotFound 常量错误）与 2 条 P2 + 若干 P3；P1/P2 已修复（commit 3bd8846），
  P3 记入 backlog（见 FINAL-ACCEPTANCE §6 与各 commit message）。
- 最终三平台门禁：Windows **921** / Linux **835** / macOS **825** lib 测试（+6/4/8/39），
  三平台 clippy `-D warnings` / fmt / doc 全干净。
- 验收标准「所有 P0/P1/P2 修复且通过对抗式复审」达成；剩余项均为已文档化的 P3 backlog
  与需真机目视的视觉确认（txt-1 hinting 取舍、win-1 重新关联 IME、win-2 跨 DPI 拖动）。
