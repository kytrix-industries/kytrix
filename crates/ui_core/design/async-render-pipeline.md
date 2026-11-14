# 三平台异步渲染管线设计（Metal / D3D12 / Vulkan）

> 状态：**已实现（2026-11-14）**；实现后的对抗性审查修订见 §15。
> 范围：`crates/ui_core/src/render/**`、`crates/ui_core/src/window.rs` 的提交段、三个平台的窗口循环提交段
> 关联：`phase-08-d3d12.md` §5、`phase-12-macos-linux.md` §5/§6、`DEVIATIONS.md` F-121/F-164/F-166..F-169、`macos-verification.md` §2.9
> 变更性质：**重写**，不保留旧路径（AGENTS §1.1）

---

## 0. 结论摘要

三个平台都改成「**UI 线程只产帧、GPU 线程只执行**」的异步管线，但**每一条异步机制都用该平台图形 API 的原生能力**，平台之间不引入任何抽象层（无 trait、无枚举、无运行时分派；共享层只放纯数据与纯函数）。

| | macOS / Metal | Windows / D3D12 | Linux / Vulkan |
|---|---|---|---|
| GPU 线程 | 每窗口一条 `kytrix-metal` 线程（无 run loop） | 每窗口一条 `kytrix-d3d12` 线程（消息泵） | 每窗口一条 `kytrix-vulkan` 线程 |
| 呈现节奏（原生） | `CAMetalDisplayLink`（主 run loop，common modes） | `DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT` 的 waitable 句柄 | `vkAcquireNextImageKHR` + Wayland 合成器节流（`MAILBOX`） |
| 完成通知 | `addCompletedHandler` + `dispatch_semaphore`(3) | `ID3D12Fence` + 每槽 fence 值 | 每帧 `VkFence` + timeline semaphore |
| 每帧阻塞点（只允许在 GPU 线程） | 3 槽 semaphore（非阻塞失败即跳帧） | waitable 句柄 + 槽 fence | `acquire`（100ms 上限，超时回循环）+ 槽 fence |
| resize 在哪个线程 | GPU 线程（只重建 persist，图层属性由主线程写） | GPU 线程（`ResizeBuffers` + 重建 persist） | GPU 线程（`oldSwapchain` 重建，无 `vkDeviceWaitIdle`） |
| UI 线程是否可能等 GPU | **否** | **否** | **否** |

**异步管线的定义（五条不变量，实现与验收都按这个判）**

1. **I1 UI 线程零 GPU 等待**：布局/绘制编码/发布帧期间不出现任何会等待 GPU 或合成器的调用（fence wait、`waitUntilCompleted`、`vkDeviceWaitIdle`、`ResizeBuffers`、阻塞式 `nextDrawable`）。
2. **I2 提交即返回**：CPU 提交后立刻返回；完成状态只通过平台原生完成通知回到渲染线程。
3. **I3 节奏由平台给**：呈现时刻由 display link / DXGI waitable / Vulkan acquire 决定，不用自建定时器冒充 vsync。
4. **I4 资源按提交序号延迟释放**：重建/扩容产生的旧资源进 retire 队列，等对应帧完成才释放；帧路径上没有「等 GPU 再释放」。
5. **I5 帧是状态不是队列**：GPU 线程每次只画最新快照；被覆盖的帧把 damage 合并进后继帧，不再用「强制全量」补救。

---

## 1. 现状审查：为什么必须重写

### 1.1 Windows / Linux：同步管线把 GPU 与合成器接在 UI 线程上

| 平台 | 代码位置 | 调用 | 后果 |
|---|---|---|---|
| Win | `render/windows/queue.rs:104`（`begin`）| 每帧开头 `WaitForSingleObject(fence[slot], INFINITE)` | 槽未完成即阻塞 UI 线程 |
| Win | `render/windows/queue.rs:134`（`submit`）| `ExecuteCommandLists` → `Signal` → **立刻等 fence 完成** | 每帧 CPU/GPU 全同步，GPU 耗时 = 主线程耗时 |
| Win | `render/windows/compositor.rs:174-181`（`resize`）| `wait_idle` + `reset_in_flight_lists` + `ResizeBuffers` | **resize 直接阻塞主线程**（这正是问题源头） |
| Win | `render/windows/compositor.rs:312` | `Present1(1, ...)` 在 UI 线程 | 呈现队列满或 DWM 慢时，`Present` 阻塞消息泵 |
| Linux | `render/linux/compositor.rs:692-697` | `vkQueueSubmit` 后立刻 `vkWaitForFences(UINT64_MAX)` | 同上，每帧全同步 |
| Linux | `render/linux/compositor.rs:182`、`render/linux/swapchain.rs:259-266` | resize 先 `queue_wait_idle` 再销毁/重建 swapchain | resize 阻塞，且把 GPU 排空 |
| Linux | `render/linux/swapchain.rs:187` | `acquire_next_image(..., u64::MAX, ...)` 在 UI 线程 | 合成器慢时 UI 线程阻塞在 acquire |

结论：这两条后端不是「异步提交」，只是「每帧把 GPU 排空」。`InFlightRing`/三槽结构在同步语义下退化成摆设（实际最多 1 帧在飞）。

### 1.2 macOS：线程方向对了，机制是自造的（mailbox 的六个错误）

当前实现（`render/macos/renderer.rs` + `queue.rs` + `swapchain.rs`）：

1. **没有跟显示器同步**：worker 由主线程的 60Hz `CFRunLoopTimer` 唤醒；`CAMetalLayer.displaySyncEnabled` 只让合成器在 present 时等 vsync，渲染本身不对齐刷新。
2. **`nextDrawable` 是阻塞调用**：文档写明「没有可用 drawable 时最多等 1 秒」（[nextDrawable](https://developer.apple.com/documentation/quartzcore/cametallayer/nextdrawable/)），所以才有 `allowsNextDrawableTimeout(true)`（1s）+ 100ms 重试 + 回队（`renderer.rs:444-478`、`queue.rs:58-73`）。这是**用重试掩盖阻塞**，不是异步。
3. **用轮询代替完成通知**：`submit` 里 `poll_completed` 逐槽查 `status()`（`queue.rs:125`），只有主线程恰好再提交时才回收。Apple 的官方模式是 `addCompletedHandler` 里 `dispatch_semaphore_signal`。
4. **背压用 `waitUntilCompleted`**：槽位复用时 `previous.buffer.waitUntilCompleted()`（`queue.rs:65`）。这是把 3 缓冲的背压点放错了对象。
5. **丢帧要「强制全量」**：被覆盖/拿不到 drawable 的帧没有画进 persist，替代帧只能整面重画（`renderer.rs:240-270`）。这个补丁只在「自造队列 + 阻塞取 drawable」下才存在。
6. **在 worker 上写 CA 图层属性**：`contentsScale`/`drawableSize` 由 worker 写（`swapchain.rs:80-98`）。Apple 没有文档保证 `CAMetalLayer` 属性的跨线程写（[CAMetalLayer](https://developer.apple.com/documentation/quartzcore/cametallayer/)），这属于未定义行为。

一句话：**mailbox 把「显示器什么时候给一帧」这件平台已经管了的事，自己重做了一遍，而且做错了。**

### 1.3 共同根因

- 帧的**生产**（布局/编码）与**消费**（GPU 编码/提交/呈现）在同一个线程，且消费端的原生阻塞点没有隔离；
- 资源重建（resize）与资源释放（retire）都在帧路径上同步做，没有按序号延迟；
- 没有任何平台用上自己的「节奏 + 背压 + 完成通知」原语。

---

## 2. 目标 / 非目标

### 2.1 目标（可验收）

1. `App::frame` / `Window::commit_frame` 在三个平台上**永不等待 GPU 或合成器**（I1）。
2. resize（含 live resize 连续拖动）不阻塞 UI 线程；拖动期间帧率由合成器/显示器决定，不出现卡顿。
3. 空闲时 0 帧、0 唤醒（GPU 线程睡在事件上，display link 暂停）。
4. GPU 慢于 CPU 时表现为**跳帧**（合并 damage），不是 UI 卡住。
5. 设备丢失/呈现失败以强类型错误回到 UI 线程（`EngineError::DeviceLost` / `Swapchain`），不 panic、不静默。
6. 离屏/快照/golden/bench 路径语义不变（同步、可读回）。

### 2.2 非目标（本次不做，避免夹带）

- 多窗口共享一个 `ID3D12Device` / `VkInstance`（当前每窗口一套，见 §3.3）。
- 多线程并行录制命令（UI 只有一条录制流）。
- HDR、`ALLOW_TEARING`、可变刷新率策略、DirectComposition 改造。
- 直接渲染进 drawable（macOS 仍保留 persist + 整面 blit，见 §5.6 的说明）。

---

## 3. 线程与所有权

### 3.1 线程模型（三平台一致的部分只是形状，机制各自实现）

```
UI 线程（App + 平台泵 + Window + 帧编码）
   │  publish(FrameRequest) / queue_atlas_upload / set_layer_scale    ← 只加锁拷贝，不等待
   ▼
每窗口 Renderer（门面，UI 线程持有）
   │  最新帧槽（最新者胜，damage 合并） + 图集上传槽 + 反馈槽
   ▼
GPU 线程（每窗口一条，独占 device/queue/swapchain/persist/upload）
   │  原生节奏原语 → 录制 → 提交 → 呈现 → 完成通知 → retire
   ▼
GPU
```

- `App`、`Window`、`NodeTree`、`InstanceBuffer` **不跨线程**：跨线程的只有 §4.1 的 `FrameRequest`（纯数据）。
- 图形对象（device/queue/swapchain/纹理/命令列表）**只属于 GPU 线程**；构造发生在 UI 线程（建窗时），构造完成后整体移交（与今天 macOS 的 `GpuRenderer` 移交方式相同）。
- CA 图层属性（macOS）是唯一例外：**只在 UI 线程写**（§5.3）。

### 3.2 哪些状态留在 UI 线程

| 状态 | 归属 | 说明 |
|---|---|---|
| `FrameScheduler` / `FrameIndex` | UI | 序号在「产出一帧」时递增；被 GPU 线程跳过的序号不产生 GPU 工作，retire 仍能正确推进 |
| `DamageAccumulator` | UI | 每帧产出的 damage 随 `FrameRequest` 交付；合并在 GPU 线程侧做（§4.2） |
| `InstanceBuffer` / `Scene` | UI | 发布时整体拷贝（`Bytes`），GPU 线程不再借用 |
| 图集像素 | UI 产、GPU 线程消费 | 整页像素 + `DirtyRows`；合并时折叠为整页（§4.2） |
| GPU 计时 / draw_calls | GPU 线程产、UI 线程 poll | HUD 允许落后一帧（与今天 macOS 一致） |
| 错误 | GPU 线程产、UI 线程 poll | 粘性：一旦 device lost，后续 poll 继续返回 |

### 3.3 为什么每窗口一条 GPU 线程（以及为什么不共享设备）

- 每窗口的 swapchain/layer、present 节奏、retire 记账天然独立；独立线程让「窗口 A 的 resize 卡住」不影响窗口 B。
- 当前实现本来就是**每窗口一套** device/instance（`windows/compositor.rs:102-110`、`linux/compositor.rs:73-110`、macOS `Device::new()`），本次沿用，不引入跨窗口共享对象。
- 多窗口共享 device 是独立议题（资源堆、图集纹理共享、队列仲裁），换掉它不影响本设计的任何不变量；不在本次范围。

---

## 4. 帧交付契约（三平台共用语义、各自实现）

> 共享的**只有纯数据与纯函数**（`render/frame.rs`、`render/compositor.rs`、`render/retire.rs`）。线程、句柄、同步原语全部在各平台模块内。这不是抽象层：三份实现仍然 `#[cfg]` 各选一份，无 trait/枚举。

### 4.1 数据结构

```rust
// render/frame.rs（共享，纯数据）
pub(crate) struct FrameRequest {
    pub index: FrameIndex,          // UI 侧提交序号
    pub payload: FramePayload,      // instances: Bytes + ops: Vec<FrameOp> + clips: Vec<GpuClipEntry>
                                    //   + geometry: Vec<GeometryDraw>（第四份数据，§4.2）
    pub plan: Plan,                 // Full | Partial(DamageRects) | Idle（来自 UI 的 damage）
    pub surface: (u32, u32),        // 物理像素（UI 的发布目标）
    pub published_at: Instant,      // HUD：latency 的起点
}
// `FramePlan` 的 `fresh` 不再由 UI 线程决定：GPU 线程用自己的「目标是新建/重建」状态
// 在渲染前构造本地 FramePlan::new(plan, fresh)，present 成功后 presented()。

// GPU 线程 → UI 线程
pub(crate) struct FrameFeedback {
    pub index: FrameIndex,
    pub stats: RenderStats,
    pub samples: Vec<GpuTimerSample>,
    // GPU 线程**观察到该帧 GPU 工作完成**的时刻（fence / timeline / completed handler），
    // 不是 Present 返回的时刻。
    pub completed_at: Instant,
    // completed_at - published_at（UI 发布 → GPU 完成），HUD 的 publish→present。
    pub latency: Duration,
}
```

`FrameFeedback` 的跨平台契约：GPU 线程必须在**观察到**完成之后才发布反馈（fence / timeline /
completed handler），按提交顺序发布，允许覆盖尚未被 UI 线程取走的上一条（反馈槽是最新者胜，
不是队列）。`latency` 与 `completed_at` 由同一个完成观测点给出，二者对同一帧满足
`latency = completed_at - published_at`。

`FrameRequest.scale` 已删除：三个平台都没有功能消费者（只在 GPU 线程做过 `scale > 0` 的
debug 断言）。DPI 的跨线程传递只保留 macOS 的 `set_layer_scale(surface, scale)`（主线程写
CA 图层，§5.3）。

`FrameOutcome::Committed(index)` 的语义随之变成「**已交付 GPU 线程**」（不等于已呈现；纯 Idle
计划被门禁跳过时也不会交付）；窗口路径的 `RenderStats`/GPU 采样从 `poll()` 取（落后 ≤1 帧），
离屏路径保持同步返回。

### 4.2 最新帧槽：合并而不是排队（I5）

UI 线程 `publish`：

1. 若已有未取走的 `FrameRequest`：保留**新**的 payload/surface，把**旧的 damage** 合并进新的 plan：任一 `Full` ⇒ `Full`；否则矩形并集，超过 `MAX_DAMAGE_RECTS` ⇒ `Full`。旧请求的 `index` 丢弃（它不会产生 GPU 工作）。
2. 替换槽位，唤醒 GPU 线程。

`FramePayload` 是**四份**一起过线的数据：`instances`（`Bytes`）+ `ops` + `clips` + `geometry`
（F-166）。`geometry` 是 `FrameOp::Geometry.index` 的下标表，**必须与 `ops` 同帧过线**：`ops`
里的下标只有配合同一帧的表才有意义，拆开或延后一帧会把几何画到错误的帧上。目前只有 Windows
的录制路径消费它（macOS / Linux 的 `FrameOp::Geometry` 还没有 GPU 缓冲来源），但三平台的
`FramePayload` 形状保持一致：它是「一帧的全部 CPU 侧数据」，不是按平台裁剪的数据包。

图集上传单独一个「每 `AtlasKind` 只留最新」的槽：新的像素覆盖旧的，`DirtyRows` 取并集（`DirtyRows::merge`，`render/atlas.rs:244`）；若发生覆盖，则把并集标记为整页（`mark(0, u32::MAX)`）——因为被丢掉的中间上传的脏行永远不会再补。上传在 GPU 线程录制下一帧时先于绘制执行。

**为什么合并 damage 是对的**：persist 纹理在 GPU 线程上跨帧保留（Windows 是 `FLIP_SEQUENTIAL` 的后备缓冲 + persist 双份，Metal/Vulkan 是 persist），被跳过的帧没有写进 persist，所以它的 damage 必须由后继帧补画。合并后不需要「强制全量」。

**代价（明说）**：每帧一次 payload 拷贝（instances + ops + clips + geometry）。这是线程边界的所有权转移，不是可选优化；1 万实例约 1.3 MB，量级远小于一次布局/文本 shaping。macOS 今天已经在付这笔拷贝（`FramePayload::from_buffer`），本次把同样的代价带到另外两个平台。

### 4.3 序号与 retire

- GPU 线程只对**真正录制并提交**的帧推进自己的 `submitted` 序号；`RetireQueue` 用 `FrameIndex` 记账（`render/retire.rs`）。
- 某帧被跳过（没有 GPU 工作）时，它的序号不会出现在完成通知里；后继帧完成时 `completed` 直接跨过它，`retire_at <= completed` 的条目照常释放——语义正确（该帧从未引用任何资源）。
- 资源重建（persist/upload 扩容）产生的旧句柄在**重建发生时**入队，tag = 当前最后提交序号；完成通知到达后才真正释放。

### 4.4 resize 请求

resize 不作为独立命令：`FrameRequest.surface` 就是目标尺寸。GPU 线程在渲染该帧前比较自己持有的 surface：

- 不同 ⇒ 先做本平台的重建（Windows `ResizeBuffers`；Vulkan 新 swapchain；Metal 只重建 persist），再渲染，并强制该帧 `Full`。
- **surface 变化时即使没有 damage 也必须发布一帧**（UI 侧 `set_size`/`set_scale` 已经会置 full damage；这条是兜底规则，避免「只有 resize、没有脏区」时目标永远不重建）。DPI 变化本身不进入 `FrameRequest`：macOS 走 `set_layer_scale`（§5.3），Windows/Linux 的物理像素由平台层算好后再作为 `surface` 出现。
- 图层/系统侧尺寸（macOS `drawableSize`、Windows 客户区）由平台层在 UI 线程同步；GPU 线程只读 drawable 纹理的实际尺寸做最后校验（§5.4）。

### 4.5 门面接口（三平台同名、各自实现）

```rust
// 窗口路径（UI 线程）
impl Renderer {
    fn new_for_<平台原语>(...) -> Result<Self, EngineError>; // 建 GPU 对象并移交 GPU 线程
    fn surface(&self) -> (u32, u32);                        // 最近发布的目标尺寸（UI 侧记录）
    fn publish(&mut self, request: FrameRequest);           // 不阻塞；合并进最新帧槽
    fn queue_atlas_upload(&mut self, kind: AtlasKind, pixels: &[u8], rows: DirtyRows);
    fn set_layer_scale(&mut self, surface: (u32, u32), scale: f32); // 仅 macOS：主线程写 CA 图层
    fn sync_pacing(&mut self, continuous: bool);            // macOS：暂停/恢复 display link；Win/Linux：空实现（事件驱动）
    fn poll(&mut self) -> Result<Option<FrameFeedback>, EngineError>;
    #[cfg(feature = "hud")] fn timing_sample(&self, section: Section) -> Option<GpuTimerSample>;
}

// 离屏路径（测试/快照/bench）：同步，保持今天的语义
impl Renderer {
    fn new_offscreen(size: (u32, u32)) -> Result<Self, EngineError>;
    fn submit(&mut self, index: FrameIndex, buffer: &InstanceBuffer,
              frame: &mut FramePlan, timings: &mut FrameTimings) -> Result<RenderStats, EngineError>;
    fn readback(&mut self) -> Result<ReadbackImage, EngineError>;
}
```

`Window::submit_gpu` 相应简化：不再调 `renderer.resize()` / `is_fresh()`，只构造 `FrameRequest` 并 `publish`；`App::poll_renderers` 从「仅 macOS」变为三平台无条件调用。

GPU 线程一旦以错误退出：`publish` 变 no-op（不 panic），`poll()` 持续返回同一错误（粘性），`Drop` 仍能 join；平台泵在下一轮把错误冒泡出去（与今天 macOS 的 `poll_renderer` 一致）。

**唤醒原语**（`publish` / `queue_atlas_upload` 之后）：Windows `SetEvent(work_event)`（auto-reset 事件）；macOS/Linux `condvar.notify_one()`（macOS 另有 display link 的 vsync 唤醒）。关闭是唯一允许 UI 线程等待 GPU 线程的时刻（`Drop` 里 signal shutdown + join），GPU 线程在退出前自行 drain 在飞帧。

---

## 5. macOS / Metal

### 5.1 结构

```
主线程                                   kytrix-metal 线程（每窗口）
─────────────────────────────────────    ─────────────────────────────────────────
CAMetalLayer（NSView 持有）
CAMetalDisplayLink ── 每 vsync ──┐
  delegate: 存 update.drawable   │      唤醒 → 取最新 FrameRequest + 最新 drawable
            唤醒 GPU 线程        │      3 槽 semaphore（非阻塞）→ 录制 → present → commit
写图层属性（drawableSize/scale） ┘      addCompletedHandler → signal + 完成入队
sync_pacing(continuous)                 retire / 计时 / 反馈
```

要点：

- display link 和图层属性留在主线程（**这是 Apple 文档保证的用法**），GPU 编码/提交/呈现全部在 GPU 线程。
- **不再调用 `nextDrawable`**：drawable 由 `CAMetalDisplayLinkUpdate.drawable` 提供（[Update](https://developer.apple.com/documentation/quartzcore/cametaldisplaylink/update)）。文档写明更新里带的 drawable 就是「这一帧要画的 drawable」。
- 主线程的 delegate 只做两件事：把 drawable 放进「最新 drawable 槽」（覆盖旧的），唤醒 GPU 线程；不在回调里编码。
- drawable 用 `Retained<ProtocolObject<dyn CAMetalDrawable>>` 持有并整体移交；Metal 对象本身线程安全，`unsafe impl Send` 的 safety 注释写成「对象线程安全 + 交接后只有 GPU 线程持有引用」。

### 5.2 每帧序列（GPU 线程）

```text
loop {
    wait(condvar)                           // 有请求/有 drawable/有完成/关闭
    drain_completions()                     // 完成通知入队者：只在这里动 retire 与计时

    if !has_pending_frame() { continue }     // 没有脏区就不画，drawable 留给下一个 vsync
    let Some(drawable) = take_latest_drawable() else { continue };
    // ① 三缓冲背压：非阻塞，失败就丢掉这个 drawable（帧请求留在槽里，下一个 vsync 再试）
    if dispatch_semaphore_wait(sem, DISPATCH_TIME_NOW) != 0 { continue; }   // I1：绝不等 GPU
    let Some(frame) = take_latest_frame() else { sem.signal(); continue };   // 防御：归还槽位

    // ② 目标校验：drawable 的实际像素尺寸 = 本帧渲染尺寸
    if persist.size != drawable.texture.size { recreate_persist(drawable.texture.size); }

    // ③ 录制（一条 command buffer）
    let cb = queue.commandBuffer();
    upload_instances_clips(slot, &frame.payload);          // slot = ring 槽（3）
    blit_atlas_uploads(&cb);                                // 本帧到期的最新图集页
    render_pass(&cb, persist, frame.plan /* Full→Clear+全画, Partial→Load+scissor */);
    blit_persist_to_drawable(&cb, persist, drawable.texture); // 整面，见 §5.6
    cb.presentDrawable(drawable);                          // 必须在 commit 前
    cb.addCompletedHandler(move |cb| {                     // 异步完成通知
        sem.signal();
        completion_tx.push(Completion { index, cb: cb.clone() });
        wake_gpu_thread();
    });
    cb.commit();                                           // 立即返回
    last_submitted = index;
}
```

- `sem` = `dispatch_semaphore_create(3)`（Apple 官方「Synchronizing CPU and GPU work」的三缓冲模式）。它保护的是**三槽上传缓冲**与 GPU 队列深度（不是 drawable 池）：在飞 command buffer 超过 3 个就会覆盖别的槽的实例字节。**等待用 `DISPATCH_TIME_NOW`**，失败即跳帧；`signal` 在 completed handler 里。
- 完成通知里不碰 retire/计时（避免跨线程改 GPU 线程状态）：只 signal + 入队 + 唤醒，真正的 `retire.on_completed` 与 `GpuTimer::resolve` 在 GPU 线程上做（`GpuTimer` 读 `GPUStartTime/GPUEndTime`，可在任意线程读，但放到 GPU 线程便于状态集中）。
- 绘制仍用今天的 persist + damage scissor；`Full` 时 `Clear`，`Partial` 时 `Load`。
- `frame.plan` 若为 `Partial` 但本帧是重建后的首帧，GPU 线程升级为 `Full`（对应今天的 `fresh`，但判定权在资源所有者手上）。

### 5.3 主线程职责（display link 与图层）

- `CAMetalDisplayLink::initWithMetalLayer:`（macOS 14.0+）在**主线程**创建；`setDelegate:`；`addToRunLoop:forMode:` 挂主 run loop 的 **common modes**（`NSRunLoopCommonModes`），目的是让 live resize（AppKit 在 `NSEventTrackingRunLoopMode` 里跑）与全屏转场期间仍然回调（[add(to:forMode:)](https://developer.apple.com/documentation/quartzcore/cametaldisplaylink/add(to:formode:))；**该行为由 §11 探针 1 实测**）。
- **部署目标必须提升到 macOS 14.0**（`CAMetalDisplayLink` 的可用性）；在 `.cargo/config.toml` 设 `MACOSX_DEPLOYMENT_TARGET=14.0`，打包的 `LSMinimumSystemVersion` 同步到 14.0。旧系统不支持是产品决策，不做运行期可用性分支（AGENTS §1.1）。
- `preferredFrameRateRange`：`CAFrameRateRange::new(30.0, screen.maximumFramesPerSecond, screen.maximumFramesPerSecond)`；系统可能因低电量/散热下调，按系统给的来。`preferredFrameLatency` 不设（文档无语义细节）。
- 图层属性（唯一写者 = 主线程）：
  - `contentsScale`、`drawableSize` 在**平台层同步窗口状态时**写（`NativeWindow::sync_state` 已知 size/scale，`renderer.set_layer_scale(surface, scale)`）；
  - 不再由 GPU 线程写（消除今天未定义行为的跨线程写）。
- `presentsWithTransaction = false`（默认，异步呈现）、`displaySyncEnabled = true`（默认，vsync）、`framebufferOnly = false`（需要 blit 进 drawable）、`maximumDrawableCount = 3`（只允许 2 或 3）。
- `sync_pacing(continuous)`（主线程调用，每次泵一轮）：
  - 需要跑：`continuous || 有未取走的帧请求 || 有在飞帧` ⇒ `setPaused(false)`；
  - 否则 `setPaused(true)`（空闲时 0 唤醒）。
  - `publish()` 里也会 `setPaused(false)`（有帧要画就必须有 vsync）。

### 5.4 resize / DPI

- 窗口尺寸变化：平台层 `sync_state` 读 AppKit 的 frame/backing scale → 主线程写 `drawableSize`/`contentsScale` → 下一帧 `FrameRequest.surface` 变化 → GPU 线程重建 persist（新纹理 + 旧句柄按 §4.3 retire）→ 该帧 `Full`。
- 不等待任何在飞帧：旧 persist 被在飞命令缓冲引用，由 retire 在完成后释放；GPU 线程立即改用新 persist。
- drawable 尺寸与请求尺寸可能差一帧（AppKit 已改、UI 还没发布新 surface）：**macOS 以 drawable 的实际尺寸为重建真值**（persist 尺寸与 `Full` 判定都由它决定）；`FrameRequest.surface` 仍保留给 UI 侧判断「是否需要发布」（§4.4 的兜底门禁），GPU 线程不拿它当 macOS 的重建依据。
- DPI 变化但物理尺寸不变：`set_layer_scale` 更新 `contentsScale`，`drawableSize` 由 AppKit 依 bounds×scale 派生（若派生滞后，下一帧按实际 drawable 尺寸重建）。

### 5.5 空闲与连续帧

- 空闲：display link `isPaused = true`；GPU 线程睡在 condvar；主线程 60Hz 动画定时器只在 `needs_continuous_frames()` 时启用（现状不变）。验收口径：空闲 2s 两次截图逐字节相同、`sample` 中主线程无 `frame_all`、GPU 线程只有一次 futex 等待。
- 连续帧（动画/HUD）：动画定时器唤醒 UI → `publish` → display link 已在跑（`sync_pacing(true)`）→ 每个 vsync 由 GPU 线程决定画或跳。

### 5.6 为什么保留 persist + 整面 blit

- drawable 的内容跨帧没有保证，drawable 还会轮换；partial damage 必须落在**持久目标**上。
- 因此窗口路径仍然是「画进 persist（按 damage scissor）→ 整面 blit → present」。这与异步无关，是 partial repaint 的前提。
- 备选「直接画进 drawable + 每帧全量重画」只在「全量重画比整面 blit 更便宜」时更优，属于后续性能议题，不在本次范围（且会改变 golden 语义）。

### 5.7 本平台明确不做

- 不调用 `nextDrawable`；不等 `waitUntilCompleted`；不轮询 `status()`；不建 mailbox/重试定时器；不在 GPU 线程写图层属性。

---

## 6. Windows / D3D12

### 6.1 结构

```
UI 线程                                  kytrix-d3d12 线程（每窗口）
────────────────────────────────────    ──────────────────────────────────────────
窗口消息（WM_SIZE 只记录尺寸）
publish(FrameRequest) ──► 最新帧槽      循环：等 work event / waitable
queue_atlas_upload ─────► 图集槽              → 取最新帧
poll() ◄──────────────── 反馈槽               → 槽 fence（通常已完成）
                                              → 录制 → Execute → Signal
                                              → Present1(1, 0, dirty)
                                              → retire / 反馈
```

### 6.2 swapchain 创建（关键 flags）

```rust
DXGI_SWAP_CHAIN_DESC1 {
    Format: DXGI_FORMAT_B8G8R8A8_UNORM,     // 现状（F-80），不改
    BufferCount: 3,                          // BUFFER_COUNT 保持
    SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, // 内容保留 ⇒ 可以只 copy dirty 区
    Scaling: DXGI_SCALING_STRETCH,
    Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
}
chain2.SetMaximumFrameLatency(2);            // 默认 1；2 = 一帧给合成器、一帧给 GPU
waitable = chain2.GetFrameLatencyWaitableObject();
```

- 该 flag 让 swapchain **自带有独立的最大排队帧信号量**；不设时同一进程内所有 swapchain 共享同一个隐式帧延迟计数（Chromium 的实测结论，见 [Chromium issue 40890504](https://issues.chromium.org/issues/40890504)）。
- 官方要求：**每帧开始渲染前等待该句柄，包括第一帧**（[GetFrameLatencyWaitableObject](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_3/nf-dxgi1_3-idxgiswapchain2-getframelatencywaitableobject)）。等待发生在 GPU 线程，不违反 I1。
- `SetMaximumFrameLatency` 必须在 `IDXGISwapChain2` 上调（不是 `IDXGIDevice1`）；`ResizeBuffers` 时 flag 必须原样传入（[DXGI_SWAP_CHAIN_FLAG](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/ne-dxgi-dxgi_swap_chain_flag)）。

### 6.3 每帧序列（GPU 线程）

```text
loop {
    // 两个等待点，各自消费自己的对象，绝不在同一帧里重复等 waitable（auto-reset，重复等会白等一个 vsync）
    if !has_pending_frame() { WaitForSingleObject(work_event, INFINITE); }   // 空闲：睡到有活
    if shutdown { break; }
    let Some(frame) = take_latest_frame() else { continue };
    if frame.surface != surface { resize_pending = Some(frame.surface); }
    if let Some(size) = resize_pending { do_resize(size); }   // 见 §6.4

    // ① 呈现背压：DXGI 的官方要求——每帧开始渲染前等 waitable（含 resize 后的第一帧）。
    //    阻塞发生在 GPU 线程，最多一个 vsync。
    WaitForSingleObject(waitable, INFINITE);

    let slot = ring.acquire(frame.index);
    queue.wait_slot_fence(slot);                            // 复用 allocator 前必须等（通常已完成）
    queue.reset_allocator_list(slot);
    record(list, slot, &frame);                             // upload / draw persist / copy to backbuffer
    list.Close();
    queue.ExecuteCommandLists(&[list]);                     // 异步，立即返回
    queue.Signal(fence, slot.signal_value);                 // 提交序号
    swapchain.present1(&mut frame_plan, /*sync*/ 1);        // 异步；失败分类见 §6.6
    frame_plan.presented();
    queue.release_completed(frame.index, &mut device);      // retire
    publish_feedback(...);
}
```

- `Present1(1, 0, dirty)`：`sync=1` 保持 vsync（与 macOS `displaySyncEnabled`、Vulkan `MAILBOX` 的观感一致：不撕裂）；dirty rect 语义完全沿用 `FramePlan`（`fresh` ⇒ 全量、矩形必须夹紧，F-72 三条继续成立）。
- 不在 UI 线程调用任何 DXGI/D3D12 的等待函数；UI 线程只在 `WM_SIZE` 更新 `Window` 尺寸并 `wake`，新尺寸随下一帧的 `FrameRequest.surface` 到达（§4.4）。
- 录制仍在一条 direct list 上（单线程录制；D3D12 的 list 本来就不是自由线程的，官方模型是「list 可并发录制、队列串行提交」，我们只有一条录制流，不需要多 list）。

### 6.4 resize 协议（GPU 线程）

```text
GPU 线程收到 FrameRequest，surface != 当前 surface：
  if resize_pending.is_some() {
      // 先把在飞帧排空到「最后提交值」——只在这里等，且只等一次；UI 线程不受影响
      wait_fence(last_signal);
      reset_in_flight_lists();                    // 丢掉对旧 back buffer 的引用
      swapchain.ResizeBuffers(0, w, h, DXGI_FORMAT_UNKNOWN,
                              FRAME_LATENCY_WAITABLE_OBJECT);   // flag 原样
      backbuffers = GetBuffer(0..3);  rtv = 重建;
      persist.recreate(w, h);                     // 旧 persist 走 retire
      let again = chain2.GetFrameLatencyWaitableObject();       // 见 §11 验证项 2
      if again != waitable { CloseHandle(waitable); waitable = again; }
      surface = (w, h); fresh = true;
  }
```

- 官方要求：`ResizeBuffers` 前必须释放全部 back buffer 引用、且 GPU 已完成相关工作（[ResizeBuffers](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/nf-dxgi-idxgiswapchain-resizebuffers)）。
- resize 后第一帧强制 `Full`（新的后备缓冲内容未定义；`FLIP_SEQUENTIAL` 只保证「同一缓冲跨 present 保留」）。

### 6.5 fence / retire

- 每槽一根 allocator+list（现状），每帧一个 fence 值；`Signal` 在 `ExecuteCommandLists` 之后、`Present1` 之前（顺序不影响正确性，但先 signal 可以让渲染线程尽快复用槽）。
- `release_completed` 只把 `<= completed` 的句柄交回 device 释放（现有 `RetireQueue` 不动）。
- `ID3D12CommandAllocator::Reset` 的前置条件不变：该槽上一次提交的 list 必须已完成（[Reset](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12commandallocator-reset)）。

### 6.6 错误与设备丢失

| 症状 | 处理 |
|---|---|
| `Present1` 返回 `DXGI_ERROR_DEVICE_REMOVED` / `DEVICE_RESET` | 粘性 `EngineError::DeviceLost`，通知 UI，停止线程 |
| `ResizeBuffers` 返回 `DXGI_ERROR_INVALID_CALL` | 说明还有引用/未等完成：记 `EngineError::Swapchain`，下一轮重试（不 panic） |
| 最小化/遮挡 | 正常 `Present1`（DWM 丢弃）；不额外节流（waitable 自己会回到可用状态） |

### 6.7 主线程职责

- `WM_SIZE`/`WM_DPICHANGED`：只更新 `Window` 的 size/scale 并 `dispatcher.wake()`（GPU 线程的 `resize_pending` 由下一帧的 `FrameRequest.surface` 派生，§4.4）。
- 不调用 `wait_idle` / `ResizeBuffers` / `Present`；窗口销毁顺序不变（先 drop `Window`→`Renderer`，再 `DestroyWindow`）。
- **关闭握手**：`Renderer::drop` 先 signal shutdown + `SetEvent(work_event)`，GPU 线程看到标志后停止 present、等自己的 fence、释放 back buffer 引用、销毁 swapchain 后退出。**UI 线程允许在关闭时等待 GPU 线程退出**（有界于消息泵），不再用 2 s 超时后 detach。UI 线程**不能直接 `join`**：DXGI 的 `Present` 可能同步 `SendMessage` 到窗口的消息泵线程（[DXGI overview](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/d3d10-graphics-programming-guide-dxgi)），直接 join 会和「消息泵停转」互等；关闭期间用 `MsgWaitForMultipleObjects`（对象 = GPU 线程句柄）**无限**边泵消息边等退出（F-169）。
- 平台泵的 1ms `SetTimer`（连续帧）保留：它是 UI 的动画时钟，不是 GPU 节奏。

---

## 7. Linux / Vulkan

### 7.1 结构

```
UI 线程（calloop：wl_display 默认队列）      kytrix-vulkan 线程（每窗口）
────────────────────────────────────────    ─────────────────────────────────────────
输入/configure/IME/剪贴板                    循环：等事件 → 取最新帧
wl_surface.frame 回调（frame_ready）              → 槽 fence → acquire（可阻塞）
publish(FrameRequest) ──► 最新帧槽                → 录制 → submit → present
poll() ◄──────────────── 反馈槽                    → 完成通知入队 → retire（timeline）
```

### 7.2 同步原语（按 Khronos 的 frames-in-flight 模式）

| 原语 | 数量 | 用途 |
|---|---|---|
| `VkFence` | 每帧槽（3） | 复用该槽的命令缓冲/上传缓冲前等待；`Submit` 时 signal |
| `image_available` 二值信号量 | 每帧槽（3） | `acquire` 时 signal，`Submit` 时 wait（`COLOR_ATTACHMENT_OUTPUT`） |
| `render_finished` 二值信号量 | **每 swapchain image** | `Submit` 时 signal，`Present` 时 wait |
| timeline 信号量 | 1 | 每帧 signal 一个值；retire 用 `vkGetSemaphoreCounterValue` 推进 |

两条硬规则：

1. `render_finished` 必须按 **image index** 索引（不能按帧槽）。presentation engine 可能在你再次 signal 同一信号量时仍持有它；只有「这张图被重新 acquire」才证明它被释放（[Vulkan Guide: Frames in flight](https://docs.vulkan.org/tutorial/latest/03_Drawing_a_triangle/03_Drawing/03_Frames_in_flight.html)）。
2. timeline 信号量**不能**用于 acquire/present（[VK_KHR_timeline_semaphore issue 7](https://docs.vulkan.org/refpages/latest/refpages/source/VK_KHR_timeline_semaphore.html)），它只用于 retire 记账。

### 7.3 每帧序列（GPU 线程）

```text
loop {
    wait(condvar)                              // 事件/关闭；允许阻塞
    drain_timeline();                          // 非阻塞推进 retire
    let Some(frame) = take_latest_frame() else { continue };
    if frame.surface != surface { recreate_swapchain(); }

    // ① 先等槽 fence，再 acquire。反过来的话：acquire 会 signal image_available[slot]，而该
    //    二值信号量可能仍被本槽上一次提交的 wait 持有，对已 signal 的信号量再次 signal 是
    //    非法用法（VUID-vkAcquireNextImageKHR-semaphore-01286）。这也是 Khronos 的
    //    frames-in-flight 顺序。fence 的复位仍只在 submit 之前（acquire 失败时不留下
    //    「已复位、无人 signal」的槽）。
    wait_for_fences(fence[slot], TRUE, UINT64_MAX);   // 允许阻塞：GPU 线程

    // ② acquire：节奏原语（I3）。timeout 用有限值（100ms）而不是 UINT64_MAX：只有这样才能在
    //    遮挡/合成器停摆时响应 shutdown。
    let image = acquire_next_image(swapchain, ACQUIRE_TIMEOUT_NS, image_available[slot], null)?;
    if image == NOT_READY { continue; }               // 回循环顶部检查 shutdown / 新帧
    // OUT_OF_DATE ⇒ 重建 swapchain 后重试一次；SUBOPTIMAL ⇒ 本帧照常，present 后重建

    reset_command_buffer(slot);
    begin_command_buffer(slot);
    upload_instances_clips(&frame);
    record_atlas_uploads();
    draw_into_persist(&frame.plan);
    copy_persist_to_image(image);                     // 整面（swapchain image 内容跨帧未定义）
    barrier(PRESENT_SRC);
    end_command_buffer();

    reset_fences(fence[slot]);                        // 复位之后必须立刻 submit
    // 本帧第一次使用 swapchain image 的是 persist→image 的 copy（TRANSFER 阶段），
    // 所以 image_available 的 wait stage 必须是 TRANSFER，不是 COLOR_ATTACHMENT_OUTPUT。
    submit(wait: image_available[slot] @ TRANSFER,
           signal: [render_finished[image], timeline_value],
           fence: fence[slot]);
    present(wait: render_finished[image], swapchain, image);   // OUT_OF_DATE/SUBOPTIMAL 处理见 §7.4
    frame.presented();
}
```

- 槽 fence 是「复用命令缓冲 / 上传缓冲」与「安全地再次 signal `image_available[slot]`」的共同前置条件，因此必须在 acquire 之前等（§7.2 的两条硬规则不因此改变）。
- **实现修订中**：平台代码的「先等槽 fence、再 acquire」顺序已与本文一致；`image_available` 的 wait stage 仍是 `COLOR_ATTACHMENT_OUTPUT`，由平台 SubAgent 改为 `TRANSFER`（F-168，§15.3）。

- 等待只发生在 GPU 线程；UI 线程的 `publish` 不碰 Vulkan。
- `vkQueuePresentKHR` 内部会 `wl_display_flush`（Mesa 实现），因此 GPU 线程必须能安全地操作连接（§7.5）。

### 7.4 swapchain 重建（不排空设备）

```text
recreate_swapchain():
    new = create_swapchain(old = current, extent = surface)   // 旧 swapchain 立即 retired
    new_images = get_swapchain_images(new)
    recreate persist（旧 persist 按 §4.3 retire）
    retired.push(current)      // 不立刻 destroy：见下
    current = new

每帧末：
    if retired 非空 && 新 swapchain 已成功 present 至少一帧 && 该帧的 fence 已完成:
        destroy retired（含其信号量）
```

规则依据：

- `oldSwapchain` 的语义：创建新 swapchain 时旧的一律 retired（即使创建失败）；**旧句柄可以随时 destroy**，但「**不能在从它 acquire 的图像的未完成操作结束前销毁**」（[vkDestroySwapchainKHR](https://docs.vulkan.org/refpages/latest/refpages/source/vkDestroySwapchainKHR.html)）。
- `vkDeviceWaitIdle` **不能**证明 presentation engine 已释放图像，所以既不需要、也不足够；本设计用「新 swapchain 的第一帧 present 完成」来保证合成器已经接手新缓冲（Khronos 的 swapchain recreation 示例采用同一策略）。
- 不引入 `VK_EXT/KHR_swapchain_maintenance1`（避免可选扩展分支）；§11 验证项 3 会实测这条规则的稳定性。

### 7.5 Wayland 线程模型（本平台特有的硬约束）

1. **WSI 有自己的事件队列**：规范要求每次 `vkCreateWaylandSurfaceKHR` 都新建一个 `wl_event_queue`，实现创建的所有 Wayland 对象都挂在这个队列上；应用**不得**自己 dispatch 这些对象（[Vulkan WSI: Wayland Platform](https://github.com/KhronosGroup/Vulkan-Docs/raw/main/chapters/VK_KHR_surface/wsi.adoc)）。Mesa 的实现与此一致，并在 `present` 里 `wl_display_flush`、在 `acquire` 里 dispatch 自己的队列。
2. **应用默认队列仍归 calloop**：输入/configure/IME/`wl_surface.frame` 回调都在 UI 线程的 `WaylandSource` 上。`wl_display` 的多线程规则是「同一时刻只有一个 reader」，libwayland 用 `reader_count` + 条件变量协调；calloop-wayland-source 已显式容忍「另一个线程先读了 socket」的情况（`WouldBlock` 分支）。
3. **帧回调请求必须早于 present**：UI 线程在 `publish` **之前**调用 `state.request_frame()`（现状已经是这个顺序），WSI 的 `attach/commit` 在 present 时发生，因此 `wl_callback.done` 一定能到达。
4. **`wl_surface` 状态变更只能在 present 之前发**：`wp_viewport.set_destination` / `set_buffer_scale` 这类窗口变更按规范必须在 `vkQueuePresentKHR` 之前发给合成器（§7.5 第 1 条的规范原文）。现有 `surface.rs` 的 `commit()` 与 WSI 的 commit 共用同一连接、由 libwayland 串行化；实现时把这些请求全部放在 UI 线程的「发布之前」段（`sync_viewport` 已经在那个位置），禁止在 present 之后补发。
5. **合成器节流优先**：即使 GPU 线程阻塞在 `acquire`，UI 线程也不受影响；帧回调让 UI 只在合成器需要时产帧。

### 7.6 present mode：`MAILBOX`

- Wayland 表面**必须**报告 `VK_PRESENT_MODE_MAILBOX_KHR`（规范原文：`vkGetPhysicalDeviceSurfacePresentModesKHR` 对 Wayland surface 必须返回 MAILBOX；[WSI 规范](https://github.com/KhronosGroup/Vulkan-Docs/raw/main/chapters/VK_KHR_surface/wsi.adoc)）。Mesa 的 Wayland WSI 也实现了它。
- 对本引擎的语义正好匹配：最新帧替换待呈现帧，不排队陈旧帧（与 §4.2 的「最新帧槽」同构）。
- `FIFO` 不用：在 Wayland 上 FIFO 会让 `acquire` 阻塞在合成器的 frame callback 上（Mesa 的 `legacy_fifo` 路径），延迟更高，且没有额外收益（MAILBOX 也不撕裂）。
- `incremental_present` 继续作为 `pNext` hint 传入（现状），不依赖它做正确性。

### 7.7 错误与设备丢失

- `VK_ERROR_DEVICE_LOST` ⇒ 粘性 `EngineError::DeviceLost`；`OUT_OF_DATE/SURFACE_LOST` 按 §7.4 重建；`SUBOPTIMAL` 只标记重建。
- `vkQueueSubmit`/`vkQueuePresentKHR` 失败一律先分类 device-lost，再落到 `EngineError::Swapchain`（沿用现状）。
- 关闭：UI 线程 signal shutdown 后**直接 `join`**（不再 2 s 超时后 detach；§6.7 的「UI 线程允许在关闭时等待」同样适用，F-169）。GPU 线程 `vkDeviceWaitIdle`（允许，退出路径）→ 销毁 swapchain/persist/命令池 → 退出。`acquire` 的 100 ms 上限保证遮挡/合成器停摆时 join 有界（最坏多等一个超时周期）。

---

## 8. 对照、阻塞点与被否决方案

### 8.1 原生机制对照（评审时按这张表核对「没有自造轮子」）

| 需求 | macOS | Windows | Linux |
|---|---|---|---|
| 提交异步 | `MTLCommandBuffer.commit` | `ExecuteCommandLists`（异步，返回 void） | `vkQueueSubmit` |
| 完成通知 | `addCompletedHandler` | `ID3D12Fence::SetEventOnCompletion` / 槽 fence | `VkFence` / timeline |
| 节奏 | `CAMetalDisplayLink` | `FRAME_LATENCY_WAITABLE_OBJECT` | `vkAcquireNextImageKHR` + 合成器 |
| 背压 | 3 drawable + semaphore(3) | `SetMaximumFrameLatency(2)` + waitable | image 数 + fence |
| resize | 重建 persist（无等待） | `ResizeBuffers`（GPU 线程） | `oldSwapchain`（无 wait idle） |
| 延迟释放 | completion handler → retire | fence 值 → retire | timeline 值 → retire |
| 局部更新 | persist + scissor + 整面 blit | persist + scissor + dirty rect copy + `Present1` | persist + scissor + 整面 copy（+ incremental hint） |

### 8.2 剩余的「会阻塞」调用（全部在 GPU 线程）

| 平台 | 调用 | 上界 | 为什么可接受 |
|---|---|---|---|
| Metal | `dispatch_semaphore_wait(NOW)` | 0（失败即跳帧） | 不阻塞 |
| D3D12 | `WaitForMultipleObjectsEx(work, waitable)` | 一个 vsync | GPU 线程就是呈现线程，这是它该等的地方 |
| D3D12 | 槽 fence | 该帧 GPU 时间 | 复用 allocator 的前置条件 |
| D3D12 | `ResizeBuffers` | 分配 + 释放旧缓冲 | 已保证 GPU 完成、无引用 |
| Vulkan | `vkAcquireNextImageKHR(100ms)` | 一个 vsync（合成器节流）；超时回循环 | 这就是 Vulkan 的节奏原语；有限超时只用来响应 shutdown |
| Vulkan | 槽 fence | 该帧 GPU 时间 | 复用命令缓冲的前置条件 |

UI 线程上没有任何一行等待。

### 8.3 被否决的方案（避免评审时重复讨论）

| 方案 | 否决原因 |
|---|---|
| 保持单线程，UI 线程 `Present(DO_NOT_WAIT)` + 延迟 `ResizeBuffers` | `ResizeBuffers` 仍然只能在「GPU 完成 + 无引用」时调，DWM 侧也可能阻塞；无法给出「UI 线程零等待」的结构保证 |
| Vulkan 单线程 + 跳帧（`timeout=0` + `vkGetFenceStatus`） | 需要为 acquire/重建各写一套「跳过—重试」状态机；Wayland 上 `acquire` 的节流语义被拆散，代码更复杂且收益只是省一条线程 |
| macOS 把 `CAMetalDisplayLink` 挂到 GPU 线程的 run loop | 文档允许任意 run loop，但 Apple 的示例/常规用法都在主 run loop；图层与 AppKit 的交互留在主线程更稳。主线程只做「收 drawable + 唤醒」，代价可忽略 |
| macOS 继续 `nextDrawable`，只把线程留着 | `nextDrawable` 的 1s 阻塞、超时重试、强制全量这一整串问题都来自它；display link 已经直接给 drawable |
| 三平台共用一个 `wgpu` 式抽象 | 直接违反「用各平台图形 API 原生能力」；且会丢失 dirty rect、`Present1`、incremental present、display link 等平台特性 |
| 保留 mailbox 的「强制全量」语义 | damage 合并（§4.2）已经能保证正确性，强制全量是多余的整面重画 |

---

## 9. 代码落点

### 9.1 新增 / 修改

| 文件 | 变更 |
|---|---|
| `render/frame.rs` | 新增 `FramePayload`（从 macOS 移入）、`FrameRequest`、`FrameFeedback`、`merge_plan` 的纯函数测试 |
| `render/compositor.rs` | 新增 `Plan::merge`（Full 优先、矩形并集、超限降级 Full）+ 单测 |
| `render/windows/renderer.rs` **新增** | 门面（最新帧槽/图集槽/反馈/线程句柄），对应 macOS 的 `renderer.rs` |
| `render/windows/queue.rs` | `submit` 去掉 `wait_for`；新增 `reset_slot`/`wait_slot_fence`；`submit_immediate` 仅测试保留 |
| `render/windows/swapchain.rs` | 加 `IDXGISwapChain2`、waitable、`SetMaximumFrameLatency(2)`、`present1`、`resize`（带 flag）、`waitable()` |
| `render/windows/compositor.rs` | 拆出 `GpuRenderer`（GPU 线程独占）；窗口循环 `gpu_loop` 落在 `render/windows/renderer.rs`（F-167）；`resize` 改由 GPU 线程调 |
| `render/linux/swapchain.rs` | 每 image 一个 `render_finished`；`create` 带 `old`；新增 `recreate`/`retire`；present mode 改 `MAILBOX` |
| `render/linux/compositor.rs` | 拆出 `GpuRenderer`（`gpu_loop` 落在 `render/linux/renderer.rs`，F-167）；`resize` 不再 `wait_idle`；提交去掉 `wait_for_fences`；retire 改 timeline 非阻塞推进 |
| `render/linux/renderer.rs` **新增** | 门面（同 Windows） |
| `render/macos/renderer.rs` | 重写：display link 委托 + 最新帧/最新 drawable 槽 + GPU 循环；删除 mailbox 的 `requeue/wait_retry/poll_completed` |
| `render/macos/queue.rs` | 删除 `Submitted` 轮询与 `waitUntilCompleted` 背压；`begin` 只取槽；`submit` 只 commit；完成走 `addCompletedHandler` |
| `render/macos/swapchain.rs` | 删除 `next_drawable`；加 display link 相关（或移到 `renderer.rs`）；保留 `drawableSize`/`scale` 设置（改由主线程调用） |
| `render/macos/compositor.rs` | `submit_payload` 改为「给定 drawable 渲染」；persist 重建不等待；图层属性写接口移到 `renderer.rs` |
| `window.rs` | `submit_gpu` 改为构造 `FrameRequest` 并 `publish`；删除 `renderer.resize`/`is_fresh` 调用；`poll_renderers` 去掉 `cfg`，三平台都调 |
| `platform/{windows,macos,linux}` 泵 | 每轮调 `poll()`；macOS 每轮调 `sync_pacing`；`WM_SIZE`/configure 只记录尺寸 |
| `lib.rs` / `render.rs` | 模块声明更新（新增两个 `renderer.rs`） |

### 9.2 删除

- `render/macos/renderer.rs` 的 `Mailbox`/`MailboxState`/`Work`/`requeue`/`wait_retry`/`FrameJob` 的邮箱语义（保留「线程 + 最新帧」这一层，但实现按 §4.2 重写）。
- `render/macos/queue.rs` 的 `Submitted` 数组、`poll_completed`、`waitUntilCompleted`。
- `render/macos/swapchain.rs` 的 `next_drawable` 与 `allowsNextDrawableTimeout`（`maximumDrawableCount = 3` 保留）。
- `render/windows/queue.rs` 的 `submit → wait_for` 同步链；`wait_idle` 在窗口路径的调用。
- `render/windows/compositor.rs` 的 `resize` 内 `wait_idle`/`reset_in_flight_lists` 的 UI 线程调用点。
- `render/linux/compositor.rs` 的 `submit_commands` 末尾 `wait_for_fences`；`resize` 内 `wait_idle`。
- `render/linux/swapchain.rs` 的 `resize` 内 `queue_wait_idle`；单一 `render_finished`。
- `render/frame.rs` 的 `InFlightRing::reserve` 的 `cfg(all(windows, ...))` 约束随 `submit_immediate` 保留情况一并复核。

---

## 10. 验收

### 10.1 自动化（三平台 CI）

- 单测：`Plan::merge`（Full 优先/并集/超限）；最新帧槽的「覆盖 + damage 合并」；图集槽的「覆盖 ⇒ 整页」；retire 跨过被跳过的序号；`sync_pacing` 状态机（纯函数化后测）。
- 现有 L1/L2（`frame_record`、`layout`、`hud`）与 L3 golden 必须**零语义变化**：它们走离屏同步路径。
- 新增 debug 断言：所有 GPU API 调用点 `debug_assert!(on_gpu_thread())`（线程 id 记在 GPU 线程上下文里），CI 跑 demo 时启用。

### 10.2 实机（每平台同一张检查表）

| 用例 | 通过判据 |
|---|---|
| live resize 连续拖动 2s | UI 线程单次 `frame` 耗时 max < 4ms；无 `ResizeBuffers`/`acquire`/`nextDrawable` 出现在 UI 线程的 sample 里；画面跟随拖动 |
| resize 到 0 宽/最小化/还原 | 不崩溃、不 device lost；恢复后首帧正确（全量） |
| 空闲 2s | 0 帧；两次截图逐字节相同；UI 线程 CPU 0.0%；GPU 线程无自旋唤醒 |
| 连续帧（HUD/动画）60s | 帧率由显示器决定（58–60 或 120）；无累积延迟（HUD 的 publish→present 延迟稳定） |
| 全屏转场（macOS）/ 遮挡（Win/Linux） | 不阻塞 UI；恢复后继续出帧 |
| 设备丢失注入（D3D12 debug layer / `VK_EXT_device_fault` / Metal 错误） | UI 线程收到粘性错误并退出，不 panic |
| 多窗口 | 各窗口独立节奏；一个窗口 resize 不改变另一个窗口帧率 |
| 关窗 | GPU 线程 join 成功；无 use-after-free（Metal validation / D3D12 debug layer / Vulkan validation 全开） |

### 10.3 测量方法

- 每平台用 `tracing`/`Instant` 记录：`publish` 耗时、GPU 线程 `record/execute/present` 耗时、完成通知延迟；HUD 增加「publish→present」一栏。
- `sample`（macOS）/ WPA（Windows）/ `perf`（Linux）确认等待只出现在 GPU 线程。

---

## 11. 实施前必须实测的三项（P0 探针）

| # | 假设 | 实验 | 不成立时的结论 |
|---|---|---|---|
| 1 | `CAMetalDisplayLink` 在主 run loop **common modes** 下，live resize / 全屏转场期间仍回调；暂停/恢复 `isPaused` 不丢 drawable | 200 行探针窗口：拖动缩放 10s + 全屏进出各 5 次，统计回调次数与 drawable 尺寸变化 | 若 live resize 期间不回调：把 display link 同时挂 `.eventTracking`（同 run loop 多 mode）再测；仍不回调则本设计的 macOS 段需要重做（评审决定），**不预置运行期分支** |
| 2 | `ResizeBuffers` 后 `GetFrameLatencyWaitableObject` 返回的句柄仍有效（或返回新句柄），`SetMaximumFrameLatency` 不需重设 | 连续 resize 200 次，每次 resize 后记录句柄值与 `GetMaximumFrameLatency`，并 present 一帧 | 若句柄失效：每次 resize 后重取并比较，不同则关闭旧的（已写进 §6.4 伪代码） |
| 3 | GPU 线程 + calloop 并发操作同一 `wl_display`（WSI 私有队列）稳定 | 探针：GPU 线程 present 10k 帧 + UI 线程同时拖动 resize 200 次；`WAYLAND_DEBUG=1` 找协议错误；用 `perf` 看 UI 线程是否自旋 | 若出现自旋/协议错误：`acquire` 改 `timeout=0`、由 frame callback 驱动重试（引入跳帧重试状态机），需评审 |

三项都只需要一次性探针代码，不进主干。

---

## 12. 需要评审确认的三点

1. **线程模型**：三平台都是「每窗口一条 GPU 线程」。若希望 macOS 保留主线程渲染（display link 回调里直接编码，不建 GPU 线程），可行且更贴近 MTKView 形态，但会失去「布局与 GPU 编码并行」，且 Windows/Linux 仍然是线程模型。**建议：三平台统一为 GPU 线程。**
2. **macOS 最低版本 14.0**（`CAMetalDisplayLink`）。替代只有 `CVDisplayLink`（macOS 15 已弃用，不在「官方推荐」之列）。**建议：提升到 14.0。**
3. **Linux present mode = `MAILBOX`**（Wayland 规范保证支持；最新帧替换待呈现帧，与 §4.2 同构，延迟最低）。若更看重「严格一帧一 vsync」，可改 `FIFO`，代价是 `acquire` 会阻塞在合成器的 frame callback 上（GPU 线程，不影响 UI）。**建议：MAILBOX。**

---

## 13. 实施顺序（建议）

1. 共享纯数据/纯函数（§4）→ 单测。
2. Windows（开发机平台）：门面 + GPU 线程 + waitable + resize 协议 → 实机验收 §10.2。
3. Linux：同上（swapchain 重建、per-image 信号量、MAILBOX）。
4. macOS：display link + 新 GPU 循环（删 mailbox）。
5. 三平台 HUD 加「publish→present」，补齐 §10 的测量。

---

## 14. 参考（官方文档）

Metal：
- [CAMetalDisplayLink](https://developer.apple.com/documentation/quartzcore/cametaldisplaylink)、[Update](https://developer.apple.com/documentation/quartzcore/cametaldisplaylink/update)、[add(to:forMode:)](https://developer.apple.com/documentation/quartzcore/cametaldisplaylink/add(to:formode:))
- [CAMetalLayer](https://developer.apple.com/documentation/quartzcore/cametallayer/)、[nextDrawable](https://developer.apple.com/documentation/quartzcore/cametallayer/nextdrawable/)、[maximumDrawableCount](https://developer.apple.com/documentation/quartzcore/cametallayer/maximumdrawablecount)
- [Synchronizing CPU and GPU work](https://developer.apple.com/documentation/metal/synchronizing-cpu-and-gpu-work)、[Triple Buffering](https://developer.apple.com/library/archive/documentation/3DDrawing/Conceptual/MTLBestPracticesGuide/TripleBuffering.html)
- [MTLCommandBuffer addCompletedHandler](https://developer.apple.com/documentation/metal/mtlcommandbuffer/addcompletedhandler(_:))、[presentDrawable](https://developer.apple.com/documentation/metal/mtlcommandbuffer/presentdrawable(_:))

D3D12 / DXGI：
- [DXGI flip model](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/dxgi-flip-model)、[DXGI_SWAP_CHAIN_FLAG](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/ne-dxgi-dxgi_swap_chain_flag)
- [GetFrameLatencyWaitableObject](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_3/nf-dxgi1_3-idxgiswapchain2-getframelatencywaitableobject)、[SetMaximumFrameLatency](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_3/nf-dxgi1_3-idxgiswapchain2-setmaximumframelatency)
- [Reduce latency with DXGI 1.3 swap chains](https://learn.microsoft.com/en-us/windows/uwp/gaming/reduce-latency-with-dxgi-1-3-swap-chains)
- [ResizeBuffers](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/nf-dxgi-idxgiswapchain-resizebuffers)、[DXGI best practices](https://learn.microsoft.com/en-us/windows/win32/direct3darticles/dxgi-best-practices)
- [ID3D12CommandAllocator::Reset](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12commandallocator-reset)、[ExecuteCommandLists](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12commandqueue-executecommandlists)
- [D3D12HelloFrameBuffering](https://github.com/microsoft/DirectX-Graphics-Samples/blob/master/Samples/Desktop/D3D12HelloWorld/src/HelloFrameBuffering/D3D12HelloFrameBuffering.cpp)

Vulkan / Wayland：
- [Frames in flight](https://docs.vulkan.org/tutorial/latest/03_Drawing_a_triangle/03_Drawing/03_Frames_in_flight.html)、[vkAcquireNextImageKHR](https://registry.khronos.org/vulkan/specs/latest/man/html/vkAcquireNextImageKHR.html)、[VkPresentModeKHR](https://docs.vulkan.org/refpages/latest/refpages/source/VkPresentModeKHR.html)
- [VkSwapchainCreateInfoKHR (oldSwapchain)](https://registry.khronos.org/vulkan/specs/latest/man/html/VkSwapchainCreateInfoKHR.html)、[vkDestroySwapchainKHR](https://docs.vulkan.org/refpages/latest/refpages/source/vkDestroySwapchainKHR.html)
- [Vulkan WSI: Wayland Platform](https://github.com/KhronosGroup/Vulkan-Docs/raw/main/chapters/VK_KHR_surface/wsi.adoc)、[swapchain_recreation sample](https://docs.vulkan.org/samples/latest/samples/api/swapchain_recreation/README.html)
- [VK_KHR_timeline_semaphore](https://docs.vulkan.org/refpages/latest/refpages/source/VK_KHR_timeline_semaphore.html)

---

## 15. 审查修订记录（2026-11-14）

实现落地后的对抗性审查发现的问题与修订。**只记录已经落进代码/文档的部分**；平台实现由后续
SubAgent 落实的条目单独标「实现修订中」（§15.3）。

### 15.1 本批已闭环

| 项 | 修订 |
|---|---|
| `FrameFeedback` 完成语义（P1） | 新增 `latency: Duration = completed_at - published_at`；`completed_at` 明确定义为 GPU 线程**观察到完成**（fence / timeline / completed handler）的时刻；写入跨平台契约：观察到完成才发布、按提交顺序发布、允许覆盖尚未被 UI 取走的上一条（§4.1）。三平台构造点先按当前发布点填 `latency`，平台 SubAgent 随后把发布点移到完成时 |
| HUD「publish→present」（P1） | HUD 增加 `Latency` 一行（放在 `Present` 之后，符合因果顺序）；`DebugHudMetrics.latency` 来自 `FrameFeedback.latency`，500 ms 平均口径与其它指标一致；`LINES` 10→11、面板高 184→200（§10.3） |
| `FrameRequest.scale` 死字段（P2） | 三平台都没有功能消费者（只剩 `scale > 0` 的 debug 断言）；删除字段、`window.rs` 的赋值与三平台断言（§4.1） |
| `FrameOutcome::Committed` 契约（P2） | 文档改为「已交付 GPU 线程」（不等于已呈现；纯 Idle 计划被门禁跳过时也不会交付）；`commit_frame` 的 `# Errors` 改为「窗口路径的 GPU 错误只在 `poll_renderer` 冒泡，`commit_frame` 只在编码/结算失败时返回 `Err`」 |
| retire 跨过被跳过的序号（P2） | 补单测：提交 1/3/5（2、4 被跳过），`on_completed(4)` 只释放 `retire_at <= 4` 的 1/3，5 保留；`on_completed(5)` 再释放 5（§4.3 / §10.1） |
| `needs_continuous_frames`（P2） | 重写为单一表达式（去掉 `if … { return true }`），修复新 nightly 的 `clippy::needless_bool`（Linux/macOS 的 `-D warnings` 会因此失败） |
| 打包最低系统版本（P2） | `Packager.toml` 增加 `[macos] minimumSystemVersion = "14.0"`，与 `.cargo/config.toml` 的 `MACOSX_DEPLOYMENT_TARGET` 一致 |

### 15.2 仍未闭环

- **live resize 探针**（§11 探针 1）：`CAMetalDisplayLink` 在 live resize / 全屏转场期间的
  回调行为只在普通窗口下验过，drag-resize 连续帧没有形成实机结论。
- **设备丢失注入**（§10.2）：三平台的错误分类已有单测，但没有 D3D12 debug layer /
  `VK_EXT_device_fault` / Metal 错误注入的实机用例。
- **多窗口**（§10.2）：每窗口一条 GPU 线程的结构已就位，但「一个窗口 resize 不影响另一个
  窗口帧率」未实测。
- **HUD 实机数值**（§10.3）：`Latency` 行与 publish→present 的 58–60 fps 口径未在三平台
  实机上采样记录。

### 15.3 实现修订中

以下条款的**文档**已按修订后的语义改写，平台代码由后续 SubAgent 落实：

- §6.7 / §7.7 的关闭握手：不再「2 s 超时后 detach」；UI 线程在关闭时允许等待 GPU 线程退出
  （Windows 用消息泵无限等，Linux/macOS 直接 `join`，有界于 `acquire` 的 100 ms 超时），F-169。
- §7.3 的 `image_available` wait stage：平台代码的「先等槽 fence、再 acquire」顺序已与本文一致，
  但 wait stage 仍是 `COLOR_ATTACHMENT_OUTPUT`，需改为 `TRANSFER`（F-168）。

