# macOS 平台与 Metal 审查

> 审查对象：`crates/ui_core/src/render/macos/*` 与 `crates/ui_core/src/platform/macos/*`，HEAD `a9a4220`。
> 环境：Windows 静态审查，**无法实机运行 macOS**；涉及系统行为、驱动行为与视觉/性能的结论均标注置信度并在「存疑 / 需实测」列出验证方法。

## 总体判断

基础架构明显高于常见自研后端：用 `CAMetalDisplayLink` 而非手写 `nextDrawable`/CVDisplayLink，窗口路径有独立 `kytrix-metal` GPU 线程、三张 drawable + 三槽 semaphore、完成回调而非 `waitUntilCompleted`、drawable/请求尺寸的显式校验与粘性退化、图层属性主线程独占。这些设计与 Apple 的 Metal 三缓冲模型、`CAMetalDisplayLink` 推荐用法方向一致。

但离“生产级 macOS 后端”仍有系统性问题，按影响排序：

1. **渲染节奏与 CPU 帧生产都锁在主 run loop 上**：display link 挂在 `NSRunLoop::mainRunLoop()`，同时布局/编码/提交也在主线程 observer 里跑。复杂行情图表的 CPU 帧工作会直接延迟 drawable 获取与发布，ProMotion 上尤其明显。
2. **ProMotion/多屏/色彩空间配置不完整**：`preferredFrameRateRange` 最小 30 Hz 会允许系统降到 30 fps；连续帧定时器写死 60 Hz；屏幕切换后不重建 display link；`CAMetalLayer.colorspace` 从未设置。
3. **“完成回调可能乱序”与“全局 semaphore 复用上传槽”的内部矛盾**：代码两处明确承认 Metal 完成回调可乱序，并用 `CompletionOrder` 处理资源 retire；但三槽上传缓冲只用“完成总数”semaphore 保护，`FrameSlot.wait_value` 未被使用。若乱序成立，后续帧可能覆盖仍被旧帧读取的上传缓冲。

此外，AppKit 的 focus 丢失输入状态清理、IME 生命周期/`replacementRange`、a11y raise 重入、主线程 join 等细节也需补齐。

**本轮未发现可静态证明的 P0**；P1 3 条、P2 8 条、P3 1 条。

---

## 发现（P0-P3）

### P1-1 渲染节奏源与 CPU 帧生产都在主 run loop，主线程繁忙会直接吃 vsync

- **证据**：
  - `crates/ui_core/src/render/macos/renderer.rs:425-427`：display link 挂到 `NSRunLoop::mainRunLoop()` 的 common modes。
  - `crates/ui_core/src/platform/macos/event_loop.rs:212-240`：`pump` 在主线程 observer/source 里执行 `app.run_frame_hook()`、`frame_all(app)`。
  - `crates/ui_core/src/platform/macos/event_loop.rs:290-298`：`frame_all` 逐个窗口调用 `app.frame(id)`（布局、文本、paint、`FramePayload` 拷贝）。
- **问题**：`CAMetalDisplayLink` 的 delegate 在主 run loop 上执行，drawable 是系统在 delegate 调用前预取的。主线程一旦被布局/绘制、`App::dispatch` 的用户回调、a11y `raise()` 或大量事件处理占住，delegate 就会晚到，GPU 线程只能在更晚的 vsync 拿 drawable，于是 present 错过目标刷新周期。GPU 线程本身被设计成“不阻塞 UI”，但节奏源却在 UI 上，I1 的隔离只解决了一半。
- **影响**：复杂图表/多窗口/HUD 连续帧时出现周期性掉帧、输入到呈现延迟抖动；主线程突发的长任务会导致成串的 skipped vsync。对高频行情应用，这是核心体验风险。
- **成熟实现对照**：Apple Game Porting Toolkit 的 `setting-up-macos-window` 明确要求“engine work 必须在专用 game/render thread，主线程延迟会阻塞 AppKit 事件与通知”，其 `render-loop-detail.md` 把 `CAMetalDisplayLink` 挂在专用 render thread 的 run loop。Chrome 的 viz/compositor、Qt 的 render thread 同样把 drawable 获取/合成节奏与 UI 线程解耦。
- **建议**：把 `CAMetalDisplayLink` 挂到 `kytrix-metal` 线程的自有 run loop（GPU 线程循环改为 run loop 驱动，或新增 render thread）；主线程只保留 `set_layer_scale` / `setPaused` 等图层属性写入。至少把 `app.frame` 的布局/编码拆到非主线程队列，主线程只做事件与图层属性。
- **置信度**：高（代码事实与 Apple 官方设计指引一致）；具体掉帧幅度需在真机用 `CAMetalDisplayLinkUpdate.targetPresentationTimestamp` / Metal HUD 实测。

### P1-2 `preferredFrameRateRange` 最小值写 30 Hz，会允许 ProMotion 在“GPU 跟得上”时降到 30 fps

- **证据**：`crates/ui_core/src/render/macos/renderer.rs:414-420`
  ```rust
  let max_frame_rate = max_frame_rate.max(30.0);
  display_link.setPreferredFrameRateRange(CAFrameRateRange::new(
      30.0,
      max_frame_rate,
      max_frame_rate,
  ));
  ```
- **问题**：把 `minimum` 设为 30 是在告诉系统“30 fps 也可接受”。Apple 的 `render-loop-detail.md` 对同一 API 的说明是：`CAFrameRateRangeMake(maxFPS, maxFPS, maxFPS)` 才锁定显示刷新率；较低的 min（例如 30）“tells the OS it may drop to that rate on ProMotion to save power — causing judder even when GPU keeps up”。
- **影响**：高刷屏上的连续动画、HUD、行情滚动可能被系统压到 30 fps；帧间隔呈 16.7/33.3 ms 交替，出现肉眼可见的 judder。这和高频交易应用对帧稳定性的要求直接冲突。
- **成熟实现对照**：Apple GPTK `render-loop-detail.md` 的 vsync-on 路径固定 `min = preferred = max = display maximumFramesPerSecond`；Chrome/Qt 的 display link 也以屏幕刷新率为 preferred/maximum。
- **建议**：默认 `CAFrameRateRange::new(max, max, max)`（max 取 `NSScreen.maximumFramesPerSecond`）；只有明确的省电模式才降低 minimum，并单独做帧率过渡策略。
- **置信度**：高（API 语义 + Apple 官方示例明确）；实际是否触发 30 fps 需在 ProMotion 真机用 Metal HUD 验证。

### P1-3 上传槽复用依赖“完成总数”semaphore，而代码自己声明完成回调可能乱序；`FrameSlot.wait_value` 未使用

- **证据**：
  - `crates/ui_core/src/render/macos/queue.rs:66-90`：`try_acquire()` 用全局 `DispatchSemaphore`（count=3）取 permit，`begin()` 由 `InFlightRing::acquire()` 轮转槽位。
  - `crates/ui_core/src/render/macos/queue.rs:221-232`：注释明确写“Apple 只保证同一队列 command buffer 的开始顺序（提交顺序），不保证完成回调的顺序”，并因此实现 `CompletionOrder`。
  - `crates/ui_core/src/render/frame.rs:235-244`、`254-286`：`FrameSlot` 带 `wait_value`，`InFlightRing` 注释说“进入本帧前必须等待到达的 fence 值”，但 `Queue::begin`/macOS 路径从未读取 `wait_value`。
  - `crates/ui_core/src/render/macos/renderer.rs:1113-1122`：`try_acquire_upload_slot()` 成功后直接 `take_frame_for_drawable` 并录制。
- **问题**：`DispatchSemaphore` 的 signal 只表示“有某一帧完成了”，不表示“本槽上一次使用的那一帧完成了”。如果完成回调真的可以乱序（代码在两处这样声明），那么第 2 帧先完成会让第 4 帧拿到 permit，而 `InFlightRing` 轮转到的槽 0 仍被第 1 帧读取；随后 `UploadArena::write`（`compositor.rs:604-638`）和 atlas staging（`atlas.rs:334-368`）会覆盖仍在被 GPU 读取的 buffer，产生逐帧闪字/错图，且 Metal validation 通常检测不到这种 CPU 覆盖。
- **反向解释**：如果实际 Metal 同一 queue 的完成顺序严格等于提交顺序，那么 `CompletionOrder`/`FeedbackQueue` 的乱序处理就是多余复杂度，注释会误导后续维护；两种情形必须二选一，不能同时成立。
- **成熟实现对照**：D3D12/Vulkan 后端均按槽位 fence/wait_value 等待（`FrameSlot.wait_value` 正是为此设计）；Apple 的三缓冲 sample 假设 frame boundary 顺序，但更稳健的实现会给每个槽独立 `MTLSharedEvent`/semaphore，并在写该槽前等待其自身完成。
- **建议**：为每个上传槽建立独立完成信号（`MTLSharedEvent` + `waitUntilSignaledValue` 或 3 个 semaphore），`begin()` 前等待 `slot.wait_value`；或者明确锁定“同队列完成顺序=提交顺序”的契约并删除 `CompletionOrder`（但需先向 Apple 文档/实测确认）。两种方案不要混用。
- **置信度**：中高。代码内部矛盾是确定的；乱序是否真实发生需在 Apple Silicon/Intel 真机用 `addCompletedHandler` 时间戳压测确认。

### P2-1 `CAMetalLayer.colorspace` 从未设置，SDR/sRGB 在广色域或多屏配置下可能颜色不一致

- **证据**：`crates/ui_core/src/render/macos/swapchain.rs:54-67` 设置了 `device`、`pixelFormat = BGRA8Unorm_sRGB`、`opaque`、`framebufferOnly = false`、`maximumDrawableCount`、`presentsWithTransaction`、`displaySyncEnabled`、`contentsGravity`、`backgroundColor`，但没有调用 `setColorspace`；全目录 grep 无 `CGColorSpace`/`colorspace`。
- **问题**：`CAMetalLayer` 的 `colorspace` 未显式指定为 `kCGColorSpaceSRGB`。Apple 的 `hdr-colorspace.md` 明确要求 SDR `BGRA8Unorm_sRGB` 与 `kCGColorSpaceSRGB` 配对；否则 Core Animation 在 P3 显示器/不同显示器配置文件下可能按层默认色彩空间解释 sRGB 编码的 drawable，出现偏色或饱和度不一致。
- **影响**：同一窗口在内置 P3 屏与外接 sRGB 屏上颜色不一致；截屏/录屏/色彩管理工具取色与引擎预期不符；后续若扩展 EDR/HDR，像素格式与色彩空间契约会直接错配。
- **成熟实现对照**：Apple GPTK `hdr-colorspace.md` 的 SDR 配置显式 `metalLayer.colorspace = CGColorSpaceCreateWithName(kCGColorSpaceSRGB)`；Chrome 的 `CAMetalLayer` 初始化同样显式设置 sRGB color space。
- **建议**：`swapchain::new_layer` 设置 `CGColorSpace::new_srgb()` 对应的 `CGColorSpace`；在 `NSWindowDidChangeScreenProfileNotification` / `NSScreenColorSpaceDidChangeNotification` 时重设并重建 drawable 参数。
- **置信度**：高（属性确实未设置）；视觉差异需真机 P3/非 P3 双屏对比确认。

### P2-2 连续帧定时器写死 60 Hz，ProMotion 120 Hz 上连续动画被限制在 60 fps

- **证据**：`crates/ui_core/src/platform/macos/window.rs:315-318`：`wake::set_animating(active, 1.0 / 60.0)`；`crates/ui_core/src/platform/macos/wake.rs:143-185` 按该 interval 创建 `CFRunLoopTimer`；`event_loop.rs:266-269` 只在该 timer tick 时调用 `frame_all`（除显式 wake 外）。
- **问题**：display link 按 `maximumFramesPerSecond`（可能是 120）运行，但主线程连续帧发布只有 60 Hz。结果是一半 vsync 没有新帧：display link 仍在每个 vsync 预取 drawable，GPU 线程发现没有 pending frame 后丢弃 drawable，画面只有 60 fps，且输入到呈现延迟按 16.7 ms 粒度抖动。`preferredFrameRateRange` 上限设 120 在这一路径上没有实际意义。
- **影响**：ProMotion 上动画/HUD/行情滚动明显不如原生 120 Hz 流畅；高刷屏用户会看到“系统支持但应用不跟手”。
- **成熟实现对照**：Apple GPTK `render-loop-detail.md` 从 `GetScreenParams().maximumFramesPerSecond` 取刷新率；Qt/Chrome 的 display link 直接以屏幕刷新率为 tick，不使用固定 60 Hz 的辅助 timer。
- **建议**：连续帧 timer interval 取 `1.0 / screen.maximumFramesPerSecond()`（并随屏幕变化重建）；更好的做法是让 display link 的 tick 直接驱动发布，去掉固定 timer 的二次节流。
- **置信度**：高（代码事实）。

### P2-3 display link 的刷新率上界和屏幕相关参数只在窗口创建时读一次，没有处理换屏/屏幕参数变化

- **证据**：`crates/ui_core/src/platform/macos/window.rs:191-198`：`maximum_frames_per_second()` 只读当前 `window.screen()`；`crates/ui_core/src/platform/macos/event_loop.rs:80-85` 只在 `Renderer::new_for_layer` 时调用一次；全目录无 `NSWindowDidChangeScreenNotification`、`NSApplicationDidChangeScreenParametersNotification`、`NSScreenColorSpaceDidChangeNotification` 观察者。
- **问题**：窗口从 60 Hz 屏幕拖到 120 Hz ProMotion（或反向）后，`preferredFrameRateRange` 仍是旧屏幕的值；从 sRGB 屏到 P3/HDR 屏后，`CAMetalLayer.colorspace`/`wantsExtendedDynamicRangeContent` 也不会更新。`viewDidChangeBackingProperties` 只更新 `ViewIvars.scale`（`view.rs:78-85`），没有通知 renderer 重建 display link。
- **影响**：多屏办公/交易员拖窗场景下高刷无法启用；专业色彩屏上颜色可能错误；外接/拔掉显示器后行为不可预测。
- **成熟实现对照**：Apple GPTK `render-loop-detail.md` 的 `displayDidChange` 在 `windowDidChangeScreen:` 与 `NSScreenColorSpaceDidChangeNotification` 时刷新 EDR headroom 并重建 frame pacing；Qt `QCocoaWindow` 注册 `NSWindowDidChangeScreenNotification` 等通知并同步 surface。
- **建议**：注册上述通知（或 `NSWindowDelegate` 的 `windowDidChangeScreen:`），在屏幕变化时重建 display link/preferred range，并重设 `colorspace`/EDR 参数；屏幕最大刷新率同时用于连续帧 timer。
- **置信度**：高（代码事实）；具体换屏行为需真机验证。

### P2-4 窗口失焦不清修饰键/按键/IME/悬停状态，Cmd-Tab 后可能留下“卡住的 Shift/Cmd”

- **证据**：
  - `crates/ui_core/src/platform/macos/window.rs:244-249`：`sync_state` 检测 `isKeyWindow` 变化后只调用 `view.focus_changed(key)`，然后发 `Event::FocusChanged`。
  - `crates/ui_core/src/platform/macos/view.rs:440-446`：`focus_changed` 只转发给 `MacA11y`，没有清 `ViewIvars.modifiers`、没有合成 key-up、没有清 IME。
  - `crates/ui_core/src/platform/macos/view.rs:466-481`：修饰键位图只由 `flagsChanged:`/鼠标/键盘事件维护，没有失焦重置路径。
- **问题**：用户按住 Shift/Cmd/Option，Cmd-Tab 到其他应用后松开，再回到本应用时 `ViewIvars.modifiers` 仍保留旧位，后续鼠标点击/快捷键会被当成带修饰键；如果按住普通键离开，也不会有 key-up。AppKit 在窗口 resign key 时不保证补发 `flagsChanged:`。
- **影响**：快捷键状态错乱（例如单击被识别为 Shift+Click、Cmd 快捷键粘滞），交易类应用中可能误触发操作；Apple GPTK 的验收清单明确要求“No stuck keys after Cmd-Tab. Focus loss clears input state.”
- **成熟实现对照**：Qt `QNSView` 在 `viewDidMoveToWindow`/`resignFirstResponder` 清理按键状态；Chromium 在 `RenderWidgetHostViewCocoa` 失焦时重置 modifier/key 状态。
- **建议**：`focus_changed(false)` 中清空 `modifiers`、`suppress_text`，清 IME marked 并推 `ImeEvent::End`，对已按下的键补发 `KeyState::Up`；同时重置 `mouseExited`/hover 状态。
- **置信度**：高（代码事实与 AppKit 行为）。

### P2-5 IME 生命周期未与 first responder/应用激活绑定，且 `replacementRange` 被忽略

- **证据**：
  - `crates/ui_core/src/platform/macos/view.rs:257-369` 实现完整 `NSTextInputClient`，但全目录没有 `NSTextInputContext::activate`/`deactivate`，也没有 `becomeFirstResponder`/`resignFirstResponder` 覆写。
  - `view.rs:260-273`：`insert_text_replacement_range(&self, text, _range)` 完全忽略 `replacementRange`。
  - `view.rs:326-334`：`attributed_substring_for_proposed_range` 恒返回 `None`；`view.rs:365-368`：`character_index_for_point` 恒返回 0。
  - `view.rs:440-446`：失焦只更新 a11y，不清理 marked text。
- **问题**：
  1. AppKit/Flutter 的实践是在文本输入开始/结束时显式 `[inputContext activate]`/`deactivate`；Kytrix 依赖 `NSView.inputContext` 自动创建，首次成为 first responder、切换窗口或应用激活状态变化时可能拿到未激活/失活的 context（AccessKit 的 AppKit 研究记录了“context 未激活时事件被 TSM 吞掉”的现象）。
  2. 失焦/窗口关闭时 `unmarkText` 不会由 AppKit 自动调用，marked text 会残留在 client 状态；重新聚焦后候选窗/预编辑可能错位。
  3. `replacementRange != NSNotFound`（输入法重转换/候选替换）被忽略，引擎按“在 caret 插入”处理，会重复插入或插到错误位置。
- **影响**：中文/日文输入法在中英切换、Cmd-Tab、重转换场景下出现丢字、重复字、候选框位置错误；对需要输入代码/搜索词的交易应用是可感知的功能缺陷。
- **成熟实现对照**：Flutter `FlutterTextInputPlugin` 在 `TextInput.show/hide` 时调用 `[_textInputContext activate]`/`deactivate`，并处理 `insertText:replacementRange:`；Qt `QCocoaInputContext` 管理 context 激活；Chromium `BridgedContentView` 在 `resignFirstResponder` 清理 IME。
- **建议**：在 view 中持有 `NSTextInputContext`（`initWithClient:self`），在 `becomeFirstResponder`/`resignFirstResponder`/窗口 key 状态变化时 activate/deactivate；失焦时 `unmarkText` 或 `discardMarkedText` 并推 `ImeEvent::End`；把 `replacementRange` 转成引擎的“替换指定 range”命令；补 `attributedSubstring` 与 `characterIndex`。
- **置信度**：高（代码缺失与 API 契约）；实际输入法行为需在启用 Pinyin/Japanese IME 的真机验证。

### P2-6 每帧整面 `persist → drawable` blit，局部更新也付全屏带宽

- **证据**：`crates/ui_core/src/render/macos/compositor.rs:454-461`：只要存在 drawable 就调用 `blit_persist_to(...)`；`compositor.rs:725-750`：`copyFromTexture...` 的 size 始终是整个 `self.surface`。`compositor.rs:7-10` 明确说明 Metal 每帧整面 blit（F-121）。
- **问题**：即使本帧只有一个 100×20 的 damage 矩形，也会把整个 persist 纹理（例如 3024×1964×4 ≈ 23.7 MiB）blit 到 drawable；60 Hz 下约 1.4 GiB/s，5K/120 Hz 可超过 4 GiB/s，占用大量内存带宽并增加 GPU 时间。对高频行情图表，GPU 时间和功耗会高于必要值。
- **影响**：ProMotion/5K 外接屏上 GPU 时间被 blit 固定成本占据，复杂场景更容易掉帧；电池/散热压力大。
- **成熟实现对照**：D3D12 后端用 `Present1` 的 dirty rects、Vulkan 用 `VK_KHR_incremental_present` 避免全屏拷贝；Metal 虽无 dirty present 扩展，但可以直接渲染到 drawable（全帧路径），或对 partial damage 使用 scissor + `framebufferOnly` 限制下的局部 blit/渲染策略，而不是无条件全屏 blit。
- **建议**：对 `Plan::Full` 直接渲染到 drawable（跳过 persist 中转）；对 partial 更新评估直接渲染到 drawable + scissor 的可行性（需确认 drawable 内容跨帧保证），或至少用 Metal HUD/GPU capture 量化 blit 成本后再决定是否保留 persist 双缓冲。
- **置信度**：高（代码事实）；实际带宽影响需真机 GPU trace。

### P2-7 `WindowRenderer::drop` 在主线程 join GPU 线程，驱动挂死时窗口关闭/退出最长卡 2 秒

- **证据**：`crates/ui_core/src/render/macos/renderer.rs:79-84`：`SHUTDOWN_GRACE = 2s`、`SHUTDOWN_POLL = 100ms`；`renderer.rs:543-563`：`Drop` 里 `setDelegate(None)`、`removeFromRunLoop`、`invalidate`、`request_shutdown()`，最后 `thread.join()`；`renderer.rs:869-899`：shutdown 时每 100 ms 轮询，累计 2 s 后 `WaitOutcome::TimedOut`。
- **问题**：窗口关闭或应用退出发生在主线程；如果 Metal 驱动没有回调完成，`Drop` 会阻塞主线程最多 2 秒（加上 100 ms 粒度）。在这期间 AppKit 事件循环停摆，用户看到“关闭按钮无响应/转菊花”，系统也可能判定应用无响应。Apple 的 render-loop 指南要求 stopRenderLoop 在 render thread 上 drain 在飞帧，不要在 UI 线程做同步等待。
- **影响**：设备丢失、驱动异常、切换 GPU、睡眠唤醒后退出时出现 2 秒卡死；对交易应用的用户体验与系统 watchdog 都不友好。
- **成熟实现对照**：Apple `render-loop-detail.md` 的 `stopRenderLoop` 在 render thread 上等待所有 `_frameSemaphore` slot，随后恢复 semaphore；主线程只做 `invalidate`/请求，不 join 渲染线程。
- **建议**：把 drain/join 移到 GPU 线程或后台 shutdown 队列；`Drop` 只发出 shutdown 请求并 detach（配合对象生命周期保证），应用退出时在 `applicationWillTerminate` 的专用阶段等待；若必须 join，主线程只等很短的 grace 并记录诊断，不要 2 s。
- **置信度**：高（代码事实）；是否真的卡住取决于驱动是否回调，需故障注入/真机验证。

### P2-8 没有构建 macOS 应用菜单（`NSMenu`/`mainMenu`），标准 Cmd 快捷键与窗口菜单缺失

- **证据**：`crates/ui_core/src/platform/macos/event_loop.rs:62-65` 只做 `NSApplication::sharedApplication`、`setActivationPolicy(Regular)`、`finishLaunching()`、`activate()`；全仓库 grep `NSMenu|setMainMenu|mainMenu` **无任何匹配**。
- **问题**：程序化启动的 AppKit 应用如果没有 main menu，就没有“应用菜单/编辑菜单/窗口菜单”：Cmd+Q、Cmd+H、Cmd+C/V/X/A、Cmd+Z、Ctrl+Cmd+F（全屏）等标准快捷键不会被处理；用户也无法从菜单发现这些操作。`GuiView` 也没有覆写 `performKeyEquivalent:`，所以这些组合键不会进入引擎的 `keyDown:` 路径。
- **影响**：不符合 macOS 平台基本交互预期；复制粘贴、退出、全屏切换等常用操作可能完全不可用；对以键盘操作为主的交易应用是明显功能缺口。Apple GPTK 的 macOS 窗口接入指南把“构建 App 菜单（About/Quit）与 Window 菜单（Toggle Full Screen）”列为 Phase 3 必做项。
- **成熟实现对照**：Apple GPTK `setting-up-macos-window` Phase 3；Qt `QCocoaMenuBar` 自动构建应用/编辑/窗口菜单；Chrome/Chromium 的 `cocoa` 初始化显式构建 main menu。
- **建议**：在 `finishLaunching` 之前构建 `NSMenu`（App/Edit/Window），把 Quit/About/Hide/Copy/Paste/Select All/Toggle Full Screen 绑定到标准 selector；如需引擎自定义 Cmd 快捷键，覆写 `performKeyEquivalent:` 并只消费未被菜单处理的组合。
- **置信度**：高（代码事实）；“Cmd+C 是否 fall through 到 keyDown”需真机按键验证，但无菜单本身已违反平台规范。

### P3-1 a11y `raise()` 期间把 adapter 取走，重入查询返回空树/空焦点

- **证据**：`crates/ui_core/src/platform/macos/view.rs:426-438`：`update_a11y` 先 `self.ivars().a11y.take()`，调用 `a11y.update(update)`，完成后才放回；`view.rs:228-252` 的 `accessibilityChildren`/`accessibilityHitTest:`/`accessibilityFocusedUIElement` 都通过 `slot.as_mut()` 访问 adapter。`crates/ui_core/src/platform/macos/a11y.rs:64-68`：`MacA11y::update` 内部 `events.raise()`，可能同步触发 NSAccessibility 查询。
- **问题**：AppKit 的 accessibility 通知可能同步回调 `accessibilityChildren`/`accessibilityFocusedUIElement`。此时 adapter 是 `None`：`children` 返回 `None`（空数组）、`hit_test` 返回 `None`、`focused` 回退为 view 自己。VoiceOver 在更新瞬间可能看到空树、焦点跳到容器，出现导航/朗读闪烁；`focus_changed` 的 `take()` 也有同样问题。
- **影响**：VoiceOver 在增量更新时偶尔读不到当前节点/焦点错乱；对依赖 a11y 的用户是明显体验问题。
- **成熟实现对照**：AccessKit 官方示例/其他平台适配器在 `raise()` 时保持 adapter 可被查询（通知与状态更新分离，或使用可重入的共享上下文）；Qt/Chromium 的 NSAccessibility 回调也会在通知后重新查询，而不是把 adapter 置空。
- **建议**：不要在 `raise()` 前把 adapter 移出 `RefCell`；改为 `try_borrow`/拆分可变状态，让查询在 raise 期间能访问稳定的只读 context；或将 `QueuedEvents` 的 `raise()` 延后到 adapter 归还之后（但需避免借用冲突），并用一个“updating”标志让查询返回旧树而不是空。
- **置信度**：中高（代码事实与 AppKit 同步回调可能性的推断）；实际 VoiceOver 行为需真机辅助功能测试。

---

## 做得好的地方

- **正确的现代节奏原语**：使用 `CAMetalDisplayLink`（`renderer.rs:411-427`），没有 `nextDrawable`/`CVDisplayLink` 手写循环；挂 common modes，覆盖 live resize/全屏转场（`wake::common_mode`、`event_loop.rs`）。
- **三缓冲链路完整**：`MAXIMUM_DRAWABLE_COUNT = 3`（`swapchain.rs:27-28`、`60`）、`MAX_FRAMES_IN_FLIGHT = 3`（`frame.rs:233`）、3 permit `DispatchSemaphore`（`queue.rs:59-71`），与 Apple 三缓冲模型对齐；窗口路径 `commit` 后立即返回（`queue.rs:93-126`），无 `waitUntilCompleted`。
- **drawable/resize 处理细致**：请求尺寸与 drawable 纹理尺寸的 ±1 px 容差、跳过陈旧 drawable、`MAX_SURFACE_MISMATCHES` 粘性退化（`renderer.rs:70-77`、`721-780`、`1090-1122`），直接针对 resize 时用旧 drawable 消费新帧导致“窗口永久停在旧尺寸”的真实问题；`scissor_for` 对 1 px 取整的边界处理（`compositor.rs:761-785`）也有测试。
- **线程所有权清晰**：图层属性只允许主线程写（`renderer.rs:222-227`、`477-486`、`swapchain.rs:88-96`），GPU 线程只读 drawable 实际尺寸；`GpuRenderer` 用 `unsafe impl Send` 表达“构造后整体移动一次、此后单线程独占”（`compositor.rs:80-83`）。
- **上传/实例数据按 in-flight 槽隔离**：`UploadArena` 每槽独立 buffer（`compositor.rs:558-638`），atlas staging 每槽独立（`atlas.rs:6-10`、`40-42`）；`RootUniform` 用 `setVertexBytes` 在录制时复制，避免共享 uniform 被旧帧读到新帧数据（`compositor.rs:472-502`）。
- **资源 retire 有完成顺序前缀**：`CompletionOrder` + `RetireQueue` 处理完成回调乱序下的旧 persist/旧上传缓冲释放（`queue.rs:221-266`）；这套机制本身是正确的（问题只在它与上传槽 semaphore 的配合，见 P1-3）。
- **事件循环方向正确**：用 `NSApplication::run()` 而不是手写 `nextEventMatchingMask`（`event_loop.rs:1-14`、`155`），让 AppKit 处理全屏转场/live resize/Space；用 `CFRunLoopSource` + common modes 跨线程唤醒（`wake.rs:59-137`），空闲时移除 timer（`wake.rs:143-185`）。
- **IME 状态质量较好**：按 UTF-16 维护 marked/selected range（`ime.rs`），区分 `NSString`/`NSAttributedString`，避免 `description` 污染预编辑串（`view.rs:517-533`），并有按键粒度的提交去重。
- **a11y 采用成熟 AccessKit**：`accesskit_macos::Adapter` + 主线程 action 回投（`a11y.rs:37-108`、`126-138`），懒激活；比自己手写 NSAccessibility 元素协议风险低。
- **D8/设备丢失处理**：启动前检查 `argumentBuffersSupport == Tier2` 与 `maxBufferLength`（`device.rs:59-108`），命令缓冲失败区分 `DeviceRemoved`/`AccessRevoked` 并写诊断（`device.rs:202-237`），错误粘性传播（`renderer.rs:297-339`）。

---

## 存疑 / 需实测

以下项目无法在 Windows 上实机确认，建议在 Apple Silicon 与 Intel Mac 上各跑一遍：

1. **Metal 完成回调的真实顺序**：在 `addCompletedHandler` 记录 `GPUStartTime`/`GPUEndTime`/到达顺序，压力下验证同队列是否可能乱序。若乱序，P1-3 的槽复用必须改为 per-slot `MTLSharedEvent`；若严格有序，可删除 `CompletionOrder` 的复杂度并修正注释。
2. **`CAMetalLayer.colorspace` 默认值**：在 P3 显示器上用同一张 sRGB 测试图对比显式 `kCGColorSpaceSRGB` 与当前实现；用 `CGDisplayCreateImage` 取色确认。
3. **display link 在主 run loop 的实际代价**：在复杂布局/大量事件场景下记录 `CAMetalDisplayLinkUpdate.targetPresentationTimestamp` 与实际 present 时间；Metal HUD 观察 FPS 与 present mode（Direct/Composited）。
4. **`setWantsLayer` 后再 `setLayer` 的 layer-hosting 行为**：真机检查 `view.layer.frame` 是否等于 `view.bounds`（Apple 文档说 layer-hosting 时 AppKit 对多数属性 hands-off；GLFW 不显式设 frame 也能工作，但 Kytrix 的调用顺序与 GLFW 相反）。若 frame 为 0/不跟随 resize，应显式设置 `layer.frame = view.bounds` 与 autoresizing mask，或改走 `makeBackingLayer` 返回 `CAMetalLayer`。
5. **`MTLBlitCommandEncoder` 的 `bytesPerRow` 对齐**：用 Metal validation 跑 `atlas.rs` 的 16² 页测试与 `readback.rs` 的 16×8 快照；若出现 row alignment 校验错误，需要把 staging 行距 pad 到 256 字节（当前 `row_bytes = page * texel`，16×4=64）。
6. **IME 真机路径**：Pinyin/Japanese IME 下验证 `insertText:replacementRange:` 的重转换、焦点丢失时的 marked text、candidate 窗口位置；检查 `inputContext` 是否在首次 `keyDown` 前已激活。
7. **`preferredFrameLatency`**：`CAMetalDisplayLink` 默认值（通常 2）会增加一帧延迟；交易应用可能希望设为 1，需在真机用 present timestamp 对比。
8. **`framebufferOnly = false` 的代价**：Apple 文档指出 `framebufferOnly = YES` 才能启用 GPU compression/direct-to-display；Kytrix 因 persist→drawable blit 必须设 false。用 Metal HUD 确认 present mode 是否为 Direct，并量化全屏 blit 的 GPU 时间。
9. **live resize / 全屏转场**：注释声称有真机 P1 实测，但静态审查无法验证 `MAX_SURFACE_MISMATCHES` 跳过期间是否出现可见的旧帧/条带；建议录屏逐帧检查（尤其 5K 外接 + 缩放模式）。
10. **a11y VoiceOver**：开启 VoiceOver 后验证增量更新期间 `accessibilityChildren`/`focusedUIElement` 的查询结果，确认 P3-1 的重入问题是否需要改为稳定快照。

---

*本报告只读审查，未修改任何源码；所有 `file:line` 均基于 HEAD `a9a4220` 的当前工作树。*
