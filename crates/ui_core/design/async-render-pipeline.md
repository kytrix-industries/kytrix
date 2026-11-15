# 三平台异步渲染管线设计（Metal / D3D12 / Vulkan）

> 状态：**已实现（2026-11-14）**；实现后的对抗性审查修订见 §15（F-166 … F-192）。
> 范围：`crates/ui_core/src/render/**`、`crates/ui_core/src/window.rs` 的提交段、三个平台的窗口循环提交段
> 关联：`phase-08-d3d12.md` §5、`phase-12-macos-linux.md` §5/§6、`DEVIATIONS.md` F-121/F-164/F-166..F-192（§27–§33）、`macos-verification.md` §2.9/§2.11/§2.12、`linux-verification.md` §4.7
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

> **状态：本节记录重写前的实现**（`523763a` / `6b5f6cd` / `16f2045` 之前）。其中的
> `nextDrawable`、`allowsNextDrawableTimeout`、`status()` 轮询、`waitUntilCompleted` 背压与
> 「强制全量」都**已被取代**（F-164、F-166 … F-182）：macOS 窗口路径改用 `CAMetalDisplayLink`
> 与 `addCompletedHandler`，`next_drawable` / `allowsNextDrawableTimeout` 已从窗口路径删除，
> damage 合并（§4.2）取代了强制全量。保留本节用于追溯「为什么必须重写」。

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
    #[cfg(target_os = "linux")]
    pub surface_local: Size,        // 逻辑尺寸（surface-local）：GPU 线程 present 前写 viewport destination
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
CA 图层，§5.3）；macOS 的真实像素尺寸不再靠 `scale` 乘出来，而是随 `surface`（真实 backing，
F-187）一起过线。

Linux 的 `surface_local` 与 `surface` 必须来自**同一帧**：`surface` 是
`round(surface_local × scale)` 的 buffer 尺寸，`surface_local` 是 compositor 显示用的逻辑尺寸
（present 前 `set_destination(ceil(...))`）。最新帧槽覆盖时两者一起替换，拆开或延后一帧会让
compositor 用错尺寸——旧 buffer 被新 destination 单独缩放（D1）或整面被 0.999× 重采样（D3）。
落点与实测见 §7.3 / §15.11、F-190 / F-191。

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

- in-flight 槽位由 `InFlightRing::acquire` 按 **acquire 次数**轮转（0 → 1 → 2 → 0），不由 `FrameIndex` 取模：最新帧槽合并会丢掉被覆盖帧的序号，序号跨度可以是任意值；Linux 的 `NOT_READY` / `OUT_OF_DATE` 重试还会多消费一次 acquire 而不提交（F-176）。
- GPU 线程只对**真正录制并提交**的帧推进自己的 `submitted` 序号；`RetireQueue` 用 `FrameIndex` 记账（`render/retire.rs`）。
- 某帧被跳过（没有 GPU 工作）时，它的序号不会出现在完成通知里；后继帧完成时 `completed` 直接跨过它，`retire_at <= completed` 的条目照常释放——语义正确（该帧从未引用任何资源）。
- 资源重建（persist/upload 扩容）产生的旧句柄在**重建发生时**入队，tag = 当前最后提交序号；完成通知到达后才真正释放。

### 4.4 resize 请求

resize 不作为独立命令：`FrameRequest.surface` 就是目标尺寸。GPU 线程在渲染该帧前比较自己持有的 surface：

- **surface 的定义分平台**（都是平台真值，不再是布局估算）：Windows = **真实客户区**（`GetClientRect` / `NativeWindow::client_size`，在 `run`、`WM_SIZE`、`WM_DPICHANGED` 之后由 `Window::set_surface` 写入；最小化时跳过，见 §6.4 / §6.7）；macOS = **真实 backing**（`NSView.convertSizeToBacking(view.bounds.size)` 的 round 值、至少 1 px，在 `sync_state` 的 Dpi / Resized 之后由 `Window::set_surface` 写入，F-187 / F-189）；Linux = **真实 Wayland buffer**（`round(logical × scale)`，`platform/linux/dpi.rs::physical_size`，在 `run` 与每轮 configure / scale 变化之后由 `Window::set_surface` 写入；fractional-scale 协议要求 round，不是 covering 的 ceil，F-191）。
- 不同 ⇒ 先做本平台的重建（Windows `ResizeBuffers`；Vulkan 新 swapchain；Metal 只重建 persist），再渲染，并强制该帧 `Full`。
- **surface 变化时即使没有 damage 也必须发布一帧**（UI 侧 `set_size`/`set_scale` 已经会置 full damage；这条是兜底规则，避免「只有 resize、没有脏区」时目标永远不重建）。DPI 变化本身不进入 `FrameRequest`：macOS 由 `set_surface` 先刷新图层（`contentsScale`/`drawableSize`）再把真实 backing 作为 `surface` 发布；Windows/Linux 的物理像素同样由平台层算好后再作为 `surface` 出现。
- **Linux 的 destination 与 buffer 同帧成对**：`FrameRequest.surface_local`（同一 UI 帧的逻辑尺寸）由 GPU 线程在 `vkQueuePresentKHR` **之前**写成 `wp_viewport.set_destination(ceil(logical))`，与 WSI 的 attach/commit 原子生效；UI 线程不再单独 commit destination。不做 seed 帧：destination 不先于 buffer 变化，旧内容不会被缩放；Wayland 的窗口尺寸由客户端 commit 决定，新 buffer 到达时一次跳到新布局（没有 Windows 的背景条中间态，F-190 / F-192）。**整数回退没有 destination**：`wl_surface.set_buffer_scale(N)` 由 GPU 线程在 present 前与 attach/commit 同一次生效，buffer = `round(logical × N)`（§15.13）。
- 图层/系统侧尺寸（macOS `drawableSize`、Windows 客户区）由平台层在 UI 线程同步；GPU 线程只读 drawable 纹理的实际尺寸做最后校验（§5.4）。
- `plan()` 的 frame 与 surface 同源；dirty rect 先与 frame 求交（越出右/下 1 px 的矩形被夹紧或丢弃），因为 `Present1` 的 dirty rect 必须完全落在 surface 内——越界是 `0x887A0001`（F-72 ②）。`paint_frame` / `commit_frame` 都走 `Window::frame_rect()`，三平台都返回 `surface`（Windows 真实客户区 / macOS 真实 backing / Linux 真实 Wayland buffer，F-186 / F-189 / F-191）；`submit_gpu` 不再自行调用 `frame_rect()`，它消费 `commit_frame` 用同一份 damage 与 frame 算好的 `plan`（目标尺寸仍取 `physical_surface()`，F-193）。

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
    drain_completions()                     // 完成通知入队者：只在这里动 retire、反馈与计时

    if !has_pending_frame() { continue }     // 没有脏区就不画，drawable 留给下一个 vsync
    let Some(drawable) = take_latest_drawable() else { continue };
    let target = drawable.texture.size;      // drawable 的实际像素尺寸

    // ① 尺寸校验必须在**占槽与取帧之前**（F-172）：drawable 可能还是上一轮 layout 的产物。
    //    不一致（>1 px，吸收 DPI 取整）⇒ 丢弃这个陈旧 drawable、跳过本 vsync，帧留在最新帧槽
    //    （pending 让 display link 保持运行）。Skip 视觉正确：旧 drawable 按 bottomLeft 贴视觉左上、
    //    保持 1:1，新条带由图层背景色补齐（§5.4）。连续 4 个 vsync 仍不一致 ⇒ **粘性退化**（F-179）：
    //    之后每个 vsync 都按 drawable 尺寸全量画，直到请求尺寸重新落回容差内；诊断只打印一次。
    let verdict = mismatch.observe(shared.published_surface(), target);   // Ready / Skip / Degraded
    if verdict == Skip { continue }
    let degrade = verdict == Degraded;

    // ② 三缓冲背压：非阻塞，失败就丢掉这个 drawable（帧请求留在槽里，下一个 vsync 再试）
    if !renderer.try_acquire_upload_slot() { continue }   // I1：绝不等 GPU
    // ③ 取帧时再校验一次尺寸（两次加锁之间可能发布了新帧）；失败则归还槽位、帧留在槽里。
    let Some(frame) = shared.take_frame_for_drawable(target, degrade) else {
        renderer.release_upload_slot();
        continue;
    };
    for upload in shared.take_atlas() { renderer.queue_atlas_upload(upload) }   // Vec<u8> 移动（F-181）

    // ④ 录制（一条 command buffer）；槽位按 acquire 次数轮转（F-176）
    let resized = renderer.prepare_target(target)?;          // persist 跟 drawable，旧句柄按 §4.3 retire
    let mut plan = FramePlan::new(frame.plan, resized || renderer.is_fresh() || degrade);
    if plan.is_idle() { renderer.release_upload_slot(); continue }   // 无像素 ⇒ 不产生 GPU 工作
    renderer.submit_frame(&frame, &drawable, &mut plan, completed_handler)?;   // commit 后立即返回
}
```

- `renderer.try_acquire_upload_slot()` 背后是 `dispatch_semaphore_create(3)`（Apple 官方「Synchronizing CPU and GPU work」的三缓冲模式）。它保护的是**三槽上传缓冲**与 GPU 队列深度（不是 drawable 池）：在飞 command buffer 超过 3 个就会覆盖别的槽的实例字节。**等待用 `DISPATCH_TIME_NOW`**，失败即跳帧；`signal` 在 completed handler 里（帧没提交时用 `release_upload_slot()` 归还）。占槽在尺寸校验之后：占槽后放弃会让 semaphore 与 ring 槽位不同源。
- 完成通知里不碰 retire/计时（避免跨线程改 GPU 线程状态）：只 signal + 入队 + 唤醒，真正的 `retire.on_completed` 与 `GpuTimer::resolve` 在 GPU 线程上做（`GpuTimer` 读 `GPUStartTime/GPUEndTime`，可在任意线程读，但放到 GPU 线程便于状态集中）。完成回调可以乱序，反馈由 `FeedbackQueue` 按 `CompletionOrder` 的连续前缀**按提交顺序**发布（F-180）。
- 绘制仍用今天的 persist + damage scissor；`Full` 时 `Clear`，`Partial` 时 `Load`。`Partial` 的 scissor 夹到目标尺寸，零面积矩形直接跳过（F-179：1 px 容差下 0 宽 scissor 会被 Metal validation 拒绝）。
- `frame.plan` 若为 `Partial` 但本帧是重建后的首帧，GPU 线程升级为 `Full`（对应今天的 `fresh`，但判定权在资源所有者手上）。

### 5.3 主线程职责（display link 与图层）

- `CAMetalDisplayLink::initWithMetalLayer:`（macOS 14.0+）在**主线程**创建；`setDelegate:`；`addToRunLoop:forMode:` 挂主 run loop 的 **common modes**（`NSRunLoopCommonModes`），目的是让 live resize（AppKit 在 `NSEventTrackingRunLoopMode` 里跑）与全屏转场期间仍然回调（[add(to:forMode:)](https://developer.apple.com/documentation/quartzcore/cametaldisplaylink/add(to:formode:))；**该行为由 §11 探针 1 实测**）。
- **部署目标必须提升到 macOS 14.0**（`CAMetalDisplayLink` 的可用性）；在 `.cargo/config.toml` 设 `MACOSX_DEPLOYMENT_TARGET=14.0`，打包的 `LSMinimumSystemVersion` 同步到 14.0。旧系统不支持是产品决策，不做运行期可用性分支（AGENTS §1.1）。
- `preferredFrameRateRange`：`CAFrameRateRange::new(30.0, screen.maximumFramesPerSecond, screen.maximumFramesPerSecond)`；系统可能因低电量/散热下调，按系统给的来。`preferredFrameLatency` 不设（文档无语义细节；本轮未验，V-121）。
- 图层属性（唯一写者 = 主线程；GPU 线程一个都不碰）：
  - `contentsGravity = kCAGravityBottomLeft`：**live resize 的关键一条**。默认 `resize` 会在 AppKit 已放大 layer bounds、而 `drawableSize` 还是旧值时把旧 drawable 拉伸/发虚（F-187）。本视图 `isFlipped = true`，CA 的 `bottomLeft` 才是**视觉左上**；实测 `TopLeft` 会把旧内容贴到左下、条带出现在上/右侧。
  - `backgroundColor` = sRGB `CGColor`（alpha 1）：旧 drawable **之外**、AppKit 刚暴露的新条带由它填（F-188 / S-141）。新 drawable **内部**的条带由 persist 的 clear color（linear）填，两处来自同一个 `Color`。
  - `contentsScale` / `drawableSize` = **真实 backing**：`NSView.convertSizeToBacking(view.bounds.size)` 的 round 值（不是 ceil，至少 1 px），由 `NativeWindow::sync_state` 每轮算一次，Dpi / Resized 之后经 `Window::set_surface` 写进图层（macOS 上 `set_surface` **先**刷新图层、再走同值 early-return）；`Window.surface` 与 `drawableSize` 是同一个整数（F-187 / F-189）。`drawableSize` **不**由 AppKit 依 bounds × scale 派生（旧文档的错误说法），也不用布局 covering。
  - `submit_gpu` 仍会在发布前调 `renderer.set_layer_scale(surface, scale)`：`surface` 此时已是平台真值，这条只作「发布的目标尺寸与图层一致」的安全网（例如平台层还没跑过 `sync_state` 的首帧）。
  - 不再由 GPU 线程写（消除今天未定义行为的跨线程写）。
- 图层的其余参数：`opaque = true`（`CAMetalLayer` 默认就是 true，仍显式写）、`masksToBounds` 默认 false 不改、`presentsWithTransaction = false`（默认，异步呈现）、`displaySyncEnabled = true`（默认，vsync）、`framebufferOnly = false`（需要 blit 进 drawable）、`maximumDrawableCount = 3`（只允许 2 或 3）、`allowsNextDrawableTimeout` 不写（`nextDrawable` 已删除，drawable 来自 display link，F-166）。
- `sync_pacing(continuous)`（主线程调用，每次泵一轮）：
  - 需要跑：`continuous || 有未取走的帧请求 || 有在飞帧` ⇒ `setPaused(false)`；
  - 否则 `setPaused(true)`（空闲时 0 唤醒）。
  - `publish()` 里也会 `setPaused(false)`（有帧要画就必须有 vsync）。

### 5.4 resize / DPI

- 窗口尺寸变化：平台层 `sync_state` 用 `NSView.convertSizeToBacking(view.bounds.size)` 读**真实 backing**（round）→ 先 `set_size`（逻辑尺寸：布局 / DPI 失效；平台真值已写入时**不再**重算 `surface`，F-194）→ 再 `set_surface(backing)`（真实 backing 的**写入口**：刷新 `contentsScale`/`drawableSize`，并把 `Window.surface` 写成真值）→ 下一帧 `FrameRequest.surface` 变化 → GPU 线程重建 persist（新纹理 + 旧句柄按 §4.3 retire）→ 该帧 `Full`。
- 不等待任何在飞帧：旧 persist 被在飞命令缓冲引用，由 retire 在完成后释放；GPU 线程立即改用新 persist。
- drawable 的实际尺寸 = 真实 backing：`drawableSize` 写的就是 `Window.surface` 的那个整数（[CAMetalLayer.drawableSize](https://developer.apple.com/documentation/quartzcore/cametallayer/drawablesize)），不是 `bounds × contentsScale` 的派生值。AppKit 已改 bounds、UI 还没发布新 surface 时两者仍可能差一帧：GPU 线程在**占槽与取帧之前**比较（1 px 容差吸收 DPI 取整）；不一致就丢弃陈旧 drawable、跳过本 vsync，帧留在最新帧槽（F-172）。persist 尺寸与 `Full` 判定以最终采用的 drawable 尺寸为准。
- **`Skip` 在 `bottomLeft` + `backgroundColor` 下视觉正确**（F-187 / F-188）：旧 drawable 保持 1:1 贴在视觉左上，新暴露的右/下区域由图层背景色补齐，所以跳一个 vsync 不会看到拉伸/发虚——这是 F-172 状态机**不改**的前提。实测（macOS 15.8.1 / Iris Plus 655 / 2x）：旧 HEAD `8598fe398` 的 stale 卡片 1624×430（1.466×/1.229× 非等比、文字 Laplacian -22%）；新代码 1108×350（1.000×/1.000× = 静止尺寸）、贴左上（y0 = 128/129、h = 174/175，连续 sweep 恒定）、卡片文字 808 vs 800（+1%）、标题 3272 vs 3279（-0.2%）、条带 RGB (13,15,19)/(14,15,19) 对静止背景 (12,14,18)（Δ≤1）。`Degraded` 只作活性保底：持续 >1 px 不一致时按 drawable 尺寸全量画，不至于永久停更。
- 缩小时旧 drawable 比 layer 大：`bottomLeft` 锚定下右/下被裁切（旧内容仍是 1:1，不出黑边/拉伸）；放大时新条带 = 背景色。两者都是接受的行为（§10.2）。
- 连续 4 个 vsync 仍不一致 ⇒ **粘性退化**（F-179）：此后每个 vsync 都按 drawable 尺寸全量渲染，直到请求尺寸重新落回容差内，诊断每次退化 episode 只打印一次。`Partial` 的 scissor 会夹到目标尺寸，零面积矩形直接跳过（1 px 容差下 0 宽 scissor 会被 Metal validation 拒绝）。
- DPI 变化但物理像素不变：`set_surface` 的图层刷新在「同值 early-return」**之前**，`contentsScale` 一定更新；`drawableSize` 不变（像素数没变），drawable 不需要重建。

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
窗口消息（WM_SIZE 只更新尺寸/客户区，不 resize）
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
    Scaling: DXGI_SCALING_NONE,              // resize 期间 DWM 不拉伸旧 buffer
    AlphaMode: DXGI_ALPHA_MODE_IGNORE,       // 背景色 alpha 不参与合成
    Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
}
raw.SetBackgroundColor(sRGB 编码的 RGBA);      // 创建后 + 每次 ResizeBuffers 后（S-139）
chain2.SetMaximumFrameLatency(1);             // CPU 排队深度 1（F-185）
waitable = chain2.GetFrameLatencyWaitableObject();
```

- `DXGI_SCALING_NONE`（[DXGI_SCALING](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/ne-dxgi1_2-dxgi_scaling)）：resize 期间 DWM **不**把旧 buffer 拉伸到新客户区，新暴露的右/下区域由 [`IDXGISwapChain1::SetBackgroundColor`](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgiswapchain1-setbackgroundcolor) 填充。旧的 `STRETCH` 实测在 300×200 跳变时把整帧放大 1.25×（card 907×269）、文字发虚（F-184）。
- **背景色编码（实测偏差，S-139）**：官方说 “we recommend that you perform floating point operations in a linear color space”，但在本路径（`B8G8R8A8_UNORM` + `FLIP_SEQUENTIAL` + `SCALING_NONE` 的 HWND swapchain）上 `DXGI_RGBA` 的 float **按 sRGB 编码字节直写**：传 linear 值得到 `(1,1,2)`，传 sRGB 编码分量得到 `(11,14,19)`（= `#0B0E13`）。实现固定走共享的 `render/color.rs::background_srgb`（`Color` → 不透明化（alpha=1、不改 RGB）→ `ColorResolver::resolve`（linear、alpha=1 ⇒ 非预乘）→ IEC 61966-2-1 linear→sRGB encode → `DXGI_RGBA`（alpha=1））；macOS 的 `NSColor`/`CGColor` 复用同一个函数（S-141）。
- **背景色契约**：`WindowOptions.background: Color`（默认 `Color::BLACK`；不透明；三平台都影响窗口底色：Windows = `SetBackgroundColor` 的 sRGB 字节，macOS = layer sRGB + clear linear，Linux = persist clear 的 **linear** 值；demo 显式设 `#0B0E13`）+ `Window::set_background_color`（运行期改写；Windows 下一次 `Present1` 生效，§6.3；macOS / Linux 见 F-188 / F-192）。官方同时说明背景色只对 windowed 的 `SCALING_NONE` swapchain 有效，且「swapchain 内容小于目标窗口时才可见」——正是 resize 期间新暴露区域；[`IDXGISwapChain1::GetBackgroundColor`](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgiswapchain1-getbackgroundcolor) 只回读存储值、不代表屏幕（§6.4）。macOS 的对应契约见 §5.3（layer sRGB + clear linear + `NSWindow` 创建期，F-188）；Linux 见 §7.3 / §7.5（S-143 / F-192）。
- 保留 `FLIP_SEQUENTIAL`（内容保留 ⇒ 可以只 copy dirty 区）与 `FRAME_LATENCY_WAITABLE_OBJECT`（§6.3 每帧等一次）。
- 该 flag 让 swapchain **自带有独立的最大排队帧信号量**；不设时同一进程内所有 swapchain 共享同一个隐式帧延迟计数（Chromium 的实测结论，见 [Chromium issue 40890504](https://issues.chromium.org/issues/40890504)）。
- 官方要求：**每帧开始渲染前等待该句柄，包括第一帧**（[GetFrameLatencyWaitableObject](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_3/nf-dxgi1_3-idxgiswapchain2-getframelatencywaitableobject)）。等待发生在 GPU 线程，不违反 I1。
- `SetMaximumFrameLatency` 必须在 `IDXGISwapChain2` 上调（不是 `IDXGIDevice1`，[SetMaximumFrameLatency](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_3/nf-dxgi1_3-idxgiswapchain2-setmaximumframelatency)）；`ResizeBuffers` 时 flag 必须原样传入（[DXGI_SWAP_CHAIN_FLAG](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/ne-dxgi-dxgi_swap_chain_flag)）。

### 6.3 每帧序列（GPU 线程）

```text
loop {
    // 两个等待点，各自消费自己的对象，绝不在同一帧里重复等 waitable（auto-reset，重复等会白等一个 vsync）
    drain_completed();                                       // 观察到完成：发布反馈 + retire（见下）
    if shutdown { break; }
    // 背景色应用点 (a)：空闲也要落地待定背景色（改色不要求提交一帧；UI 只写共享槽 + wake）。
    if let Some(rgba) = shared.take_background() { swapchain.set_background_color(rgba)?; }
    if !has_pending_frame() {
        // 空闲：work（新帧/关闭）或 completion（在飞帧完成）任一到达；两者都是 auto-reset。
        WaitForMultipleObjects([work_event, completion_event], INFINITE);
        continue;
    }
    // ① 呈现背压：DXGI 的官方要求——每帧开始渲染前等 waitable（含 resize 后的第一帧）。
    //    阻塞发生在 GPU 线程，最多一个 vsync。等待期间 completion 到达 ⇒ 先 drain 再继续等同一个
    //    waitable（auto-reset 只能消费一次）。**先等 waitable 再取帧**：渲染的是「此刻最新」的
    //    快照，而不是最多等了一个 vsync 的旧快照（§4.2 的「帧是状态不是队列」）。
    loop {
        if WaitForMultipleObjects([waitable, completion_event], INFINITE) == waitable { break; }
        drain_completed();                  // completion 分支：帧完成，先发反馈再继续等
        if shutdown { return; }             // 关闭：`request_shutdown` 置位 completion 把这里唤醒；
                                            // waitable 没被消费，不能再等它（驱动/遮挡会让关闭挂死）
    }
    if shutdown { return; }             // 关闭：waitable 即使刚被消费也不 present（swapchain 随即
                                        // 销毁，没有下一帧需要这个令牌）；这是 token 守恒的**唯一**
                                        // 例外（F-182）：其余两个不画出口仍是断言，不是可走路径。
    // **token 守恒：消费 waitable ⇒ 本帧必须 present。** 两个「不画」的出口（取帧失败、Idle 帧）
    // 都必须在消费之前判掉：`has_pending_frame()` 已保证取得到帧（帧槽只有本线程消费），而
    // `window.rs` 在目标未重建时根本不发布 Idle 计划（surface 变化会先 resize 并把 fresh 置真）。
    // 实现里这两个出口保留为不变量断言（`debug_assert!`），不是可走路径。
    let Some(frame) = take_latest_frame() else { unreachable!("pending frame was checked") };
    if frame.plan == Idle && !is_fresh { unreachable!("idle frames are never published") };
    // 背景色应用点 (b)：等 waitable 期间到达的背景色与帧一起落地（仍在 resize / 录制之前）。
    if let Some(rgba) = shared.take_background() { swapchain.set_background_color(rgba)?; }
    if frame.surface != surface { do_resize(frame.surface)?; }   // 见 §6.4；任何错误 ⇒ 显式失败

    let slot = ring.acquire();                              // 按 acquire 次数轮转，不由 FrameIndex 取模（F-176）
    queue.wait_slot_fence(slot);                            // 复用 allocator 前必须等（通常已完成）
    drain_completed();                                      // 该槽上一帧的 timestamp 必须在被覆盖前读走
    queue.reset_allocator_list(slot);
    record(list, slot, &frame);                             // upload / draw persist / copy to backbuffer
    list.Close();
    queue.ExecuteCommandLists(&[list]);                     // 异步，立即返回
    queue.Signal(fence, slot.signal_value);                 // 提交序号
    swapchain.present1(&mut frame_plan, /*sync*/ 1);        // 异步；失败分类见 §6.6
    frame_plan.presented();
    // 不在提交点发布反馈：把本帧的统计与发布时刻记账，等 fence 的完成通知。
    pending.push_back({ index, stats, cpu_samples, published_at });
    fence.SetEventOnCompletion(slot.signal_value, completion_event);
}

// 只允许在「观察到该帧完成」之后执行（fence 值 / SetEventOnCompletion 醒来）：
drain_completed():
    while pending.front().signal_value <= fence.GetCompletedValue():
        frame = pending.pop_front();
        samples = frame.cpu_samples + timer.resolve(frame.slot);   // 该槽的 timestamp 不再被写
        completed_at = Instant::now();
        publish_feedback(FrameFeedback {
            index, stats, samples,
            completed_at,                                          // GPU 线程观察到完成
            latency: completed_at - frame.published_at,
        });
    queue.release_completed(&mut device);                      // retire（同一观测点，推进到已完成值）
```

- `Present1(1, 0, dirty)`：`sync=1` 保持 vsync（与 macOS `displaySyncEnabled`、Vulkan `MAILBOX` 的观感一致：不撕裂）；dirty rect 语义完全沿用 `FramePlan`（`fresh` ⇒ 全量、矩形必须夹紧，F-72 三条继续成立）。
- GPU 循环里只有两个 `take_background` 应用点：空闲（等待 work/completion 之前）与取帧之后、resize/录制之前（伪代码 (a) (b)）；`ResizeBuffers` 内的重设是同一存储值的重新应用，不是新的取色点。`SetBackgroundColor` 只在下一次 `Present1` 生效（官方），因此 UI 线程任意时刻改色都合法，实际可见时间由 GPU 线程的下一次应用点 + present 决定；`shutdown` / `terminated` 之后的改色请求直接丢弃（`Shared::set_background` no-op），GPU 线程退出前不再应用待定色。应用背景色不消费 waitable、不是 present 出口，token 守恒不变（F-182 的关闭例外是唯一例外）。
- 不在 UI 线程调用任何 DXGI/D3D12 的等待函数；UI 线程只在 `WM_SIZE`/`WM_DPICHANGED` 更新 `Window` 尺寸、scale 与真实客户区 surface（`set_surface`）并 `wake`，新尺寸随下一帧的 `FrameRequest.surface` 到达（§4.4 / §6.7）。
- 反馈与 retire 都只在完成观测点推进：提交点只做 `SetEventOnCompletion`（§4.1 的跨平台契约；`completed_at` 不是 `Present1` 返回的时刻）。
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
      swapchain.set_background_color(background);               // 官方未承诺保留 ⇒ 无条件重设（S-139）
      persist.recreate(w, h);                     // 旧 persist 走 retire
      let again = chain2.GetFrameLatencyWaitableObject();       // 见 §11 验证项 2
      if again != waitable { CloseHandle(waitable); waitable = again; }
      surface = (w, h); fresh = true;
  }
```

- 官方要求：`ResizeBuffers` 前必须释放全部 back buffer 引用、且 GPU 已完成相关工作（[ResizeBuffers](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/nf-dxgi-idxgiswapchain-resizebuffers)）。
- resize 后第一帧强制 `Full`（新的后备缓冲内容未定义；`FLIP_SEQUENTIAL` 只保证「同一缓冲跨 present 保留」）。
- **resize 目标 = 真实客户区**：`w, h` 来自 `FrameRequest.surface`，即平台层写入的 `Window.surface`（§4.4 / §6.7），不再是 `size × scale` 的 covering。`Window::set_surface` 在 `WM_SIZE` 后直接用 lParam 的客户区物理尺寸，`WM_DPICHANGED` 后回读 `client_size()`；最小化时 `GetClientRect` 是 0×0，不拿它当目标，**不重建 1×1**（§6.7 的 `IsIconic` guard）。
- `ResizeBuffers` 之后**无条件重设背景色**：官方没有承诺 `ResizeBuffers` 保留 `SetBackgroundColor`（[GetBackgroundColor](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgiswapchain1-getbackgroundcolor) 只回读存储值），所以 `Swapchain::resize` 末尾总调 `apply_background`，不依赖重建前的值（S-139）。
- 实现不在 resize 后重设 `SetMaximumFrameLatency`；waitable 句柄按上面的伪代码重取（§11 验证项 2）。

### 6.5 fence / retire

- 每槽一根 allocator+list（现状），每帧一个 fence 值；`Signal` 在 `ExecuteCommandLists` 之后、`Present1` 之前（顺序不影响正确性，但先 signal 可以让渲染线程尽快复用槽）。
- `release_completed` 只把 `<= completed` 的句柄交回 device 释放（现有 `RetireQueue` 不动）。
- `ID3D12CommandAllocator::Reset` 的前置条件不变：该槽上一次提交的 list 必须已完成（[Reset](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12commandallocator-reset)）。

### 6.6 错误与设备丢失

| 症状 | 处理 |
|---|---|
| `Present1` 返回 `DXGI_ERROR_DEVICE_REMOVED` / `DEVICE_RESET` | 粘性 `EngineError::DeviceLost`，通知 UI，停止线程 |
| `ResizeBuffers` 返回 `DXGI_ERROR_INVALID_CALL`（或 resize 链上任何其它错误） | **显式失败**：`GpuRenderer::resize` 的每一步都先走 `classify()`（DeviceLost 优先归类），GPU 循环收到任何 resize 错误都 `publish_error` + 置粘性错误 + 退出线程。**不重试、不丢弃已取走的帧**（与 Linux/macOS 一致） |
| 最小化/遮挡 | 正常 `Present1`（DWM 丢弃）；不额外节流（waitable 自己会回到可用状态） |

`ResizeBuffers` 的 `DXGI_ERROR_INVALID_CALL` 只可能来自「还有引用 / GPU 未完成」这类**实现错误**：
本设计的前置条件（§6.4 的 `wait_fence(last_signal)` + `reset_in_flight_lists()` + 按帧重新
`GetBuffer`）已经保证二者成立，用户输入无法触发它。因此「下一轮重试」是错误策略：surface 已经
变化而目标没有重建，每一帧都会在同一个调用上失败，UI 只会看到画面不再更新（无声失败）；重试
也掩盖了真正的引用泄漏。把 resize 错误统一成显式失败后，「不静默、不冻结、不丢帧」三条同时
成立：UI 在 `poll_renderer` 里拿到粘性错误并退出。

### 6.7 主线程职责

- `WM_SIZE`/`WM_DPICHANGED`：更新 `Window` 的 size/scale 与**真实客户区 surface**（`set_surface`；`WM_SIZE` 的 lParam 就是新客户区的物理宽高，`WM_DPICHANGED` 回读 `client_size()` 且最小化时用 `IsIconic` 跳过），然后 `dispatcher.wake()`。GPU 线程的目标尺寸由下一帧的 `FrameRequest.surface` 派生（§4.4）。
- 不调用 `wait_idle` / `ResizeBuffers` / `Present`；窗口销毁顺序不变（先 drop `Window`→`Renderer`，再 `DestroyWindow`）。
- 背景色：`Window::set_background_color` 只在 UI 线程写 `Shared` 的待应用槽 + `wake`（并标 full damage），不在 UI 线程调 `SetBackgroundColor` 或任何 DXGI（I1）；GPU 线程在 §6.3 的两个应用点落地，下一次 `Present1` 生效。编码走共享 `render/color.rs::background_srgb`（S-139）。macOS 已接同一契约（layer sRGB + clear linear，F-188 / S-141）；Linux 也已接（persist clear 的 **linear** 值 + `Shared` 背景槽，F-192 / S-143）。
- **关闭握手**：`Renderer::drop` 先 signal shutdown + `SetEvent(work_event)` + `SetEvent(completion_event)`（GPU 线程可能阻塞在 `[waitable, completion]` 上，`work` 不在那个等待集合里），GPU 线程看到标志后停止 present、等自己的 fence、释放 back buffer 引用、销毁 swapchain 后退出。**UI 线程允许在关闭时等待 GPU 线程退出**（有界于消息泵），不再用 2 s 超时后 detach。UI 线程**不能直接 `join`**：DXGI 的 `Present` 可能同步 `SendMessage` 到窗口的消息泵线程（[DXGI overview](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/d3d10-graphics-programming-guide-dxgi)），直接 join 会和「消息泵停转」互等；关闭期间用 `MsgWaitForMultipleObjects`（对象 = GPU 线程句柄）**无限**边泵消息边等退出（F-169）。
- 平台泵的 1ms `SetTimer`（连续帧）保留：它是 UI 的动画时钟，不是 GPU 节奏。

---

## 7. Linux / Vulkan

### 7.1 结构

```
UI 线程（calloop：wl_display 默认队列）      kytrix-vulkan 线程（每窗口）
────────────────────────────────────────    ─────────────────────────────────────────
输入/configure/IME/剪贴板                    循环：等事件 → 取最新帧
wl_surface.frame 回调（frame_ready）              → 槽 fence → acquire（可阻塞）
publish(FrameRequest{surface,                  → 录制 → set_destination（present 前）
  surface_local}) ──► 最新帧槽                 → submit → present（attach + destination 原子）
poll() ◄──────────────── 反馈槽                → 完成通知入队 → retire（timeline）
```

UI 线程**不再 commit `wl_surface`**（唯一例外是建窗的首个 commit，用来拿到首个 configure）；
`wp_viewport` 在 `WaylandState::take_viewport()` 之后由 GPU 线程持有，destination 与 buffer 在
present 里同一次生效（§7.5 / F-190）。

### 7.2 同步原语（按 Khronos 的 frames-in-flight 模式）

| 原语 | 数量 | 用途 |
|---|---|---|
| `VkFence` | 每帧槽（3） | 复用该槽的命令缓冲/上传缓冲前等待；`Submit` 时 signal |
| `image_available` 二值信号量 | 每帧槽（3） | `acquire` 时 signal，`Submit` 时 wait（`TRANSFER`：swapchain image 的首次使用是 layout transition + 整面 copy） |
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
    // frame.surface = round(logical × scale)（平台层 set_surface 写入）；不是 covering。
    if frame.surface != surface { recreate_swapchain(); }

    // ⓪ 取本帧槽位：按 **acquire 次数**轮转（0 → 1 → 2 → 0），不由帧序号取模（F-176）。
    //    重试（NOT_READY / OUT_OF_DATE）会消费一次 acquire 而不提交，下一次自然落到下一个槽。
    let slot = ring.acquire();

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

    // ③b destination：必须在 vkQueuePresentKHR **之前**写（WSI 规范要求窗口系统变更先于
    //    present；present 内部的 `wl_surface.commit` 让 attach 与 destination **同一次**生效）。
    //    只在变化时下发；`frame.surface_local` 与 `frame.surface` 来自同一 UI 帧，旧 buffer 不会
    //    被新 destination 单独缩放（D1 / D2）。没有 viewporter ⇒ 整数回退：不写 destination，
    //    改在 present 前写 wl_surface.set_buffer_scale(N)（§15.13）。
    if let Some(viewport) = viewport {
        if let Some((width, height)) = destination_update(last_destination, frame.surface_local) {
            viewport.set_destination(width, height);   // ceil(logical)，至少 1 px
            last_destination = Some((width, height));
        }
    }

    present(wait: render_finished[image], swapchain, image);   // OUT_OF_DATE/SUBOPTIMAL 处理见 §7.4
    frame.presented();
}
```

- **destination 与 buffer 的唯一配点**：`wp_viewport` 从 UI 线程移交后只由 GPU 线程使用；`set_destination` 写的是**逻辑尺寸**（`ceil(logical)`，至少 1 px），buffer 是 `round(logical × scale)`，两者按上面的顺序在同一次 present commit 里原子生效。没有 viewporter 时没有 destination，改由 GPU 线程在 present 前写 `wl_surface.set_buffer_scale(N)`（整数回退，buffer = `round(logical × N)`，§15.13）。`Drop` 时先 `viewport.destroy()`：正常路径下 renderer 在 `WaylandState` 之前 drop，销毁时 `wl_surface` 仍存活（例外：renderer 挂载后的 fallible setup 早退路径，V-122 ⑨ / P2-3，已由 §34 F-196 闭环）。
- **UI 线程不 commit `wl_surface`**：删除 `sync_viewport` / `viewport_applied` / `set_buffer_scale(1)` 与 IME 两处 surface commit 后，`wl_surface.frame` 回调只由 present 触发，FrameGate 的「≤1 在飞」重新成立（D4）。`zwp_text_input_v3` 的状态在它自己的 `commit` 生效（v1 不需要 surface commit）。
- **背景 clear**：persist 的 `clear_color` 是窗口背景色（`render/color.rs::background_linear`，按 **linear** 写 Vulkan sRGB attachment，S-143）；UI 线程只写 `Shared` 背景槽 + `wake`（并标 full damage），GPU 线程在空闲或取帧后应用（`wait_for_work` 把待应用色算工作）。下一次全量录制生效；**调用方需在之后触发一帧**（与 Windows 相同）；不引入 seed 帧（F-192）。

- 槽 fence 是「复用命令缓冲 / 上传缓冲」与「安全地再次 signal `image_available[slot]`」的共同前置条件，因此必须在 acquire 之前等（§7.2 的两条硬规则不因此改变）。
- `image_available` 的 wait stage 与 swapchain image 首个 layout transition 的 `srcStageMask` **都必须是 `TRANSFER`**（dst 已经是 `TRANSFER`）：本后端不直接往 swapchain image 上画，首次访问是 transition + 整面 copy。写成 `COLOR_ATTACHMENT_OUTPUT` / `TOP_OF_PIPE` 时 transition 与 acquire 信号量没有依赖链，syncval 报 `SYNC-HAZARD-WRITE-AFTER-READ`（F-168，§15.4）。
- 反馈只在**观察到 timeline 完成**时发布（§15.4）：每轮循环先 `vkGetSemaphoreCounterValue` 推进 retire，再按提交顺序发布 `timeline_value <= 完成值` 的帧（`timeline_value` 是本帧 submit 时 signal 的值 = 帧序号，**不是** ring 的提交计数——帧序号会跳号，两者不同源）；有在飞帧时 condvar 用 4 ms 超时（完成不会唤醒 condvar），否则无限等待（0 自旋）。

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
3. **帧回调请求必须早于 present，且只能由 present 触发**：UI 线程在 `publish` **之前**调用 `state.request_frame()`，WSI 的 `attach/commit` 在 present 时发生，因此 `wl_callback.done` 一定能到达；`wl_surface.frame` 只在 `wl_surface.commit` 之后生效，而 UI 线程已不再 commit（见第 4 条），所以 FrameGate 的「≤1 在飞」不会被提前回调破坏（D4）。`zwp_text_input_v3` 的状态在它自己的 `commit` 生效，v1 不需要 surface commit。
4. **`wl_surface` 状态变更只由 GPU 线程在 present 之前发**：`wp_viewport.set_destination` 这类窗口变更按规范必须在 `vkQueuePresentKHR` 之前发给合成器。建窗后 `WpViewport` 经 `WaylandState::take_viewport()` 交给 GPU 线程（`Renderer::new_for_surface(..., viewport)`），由 `GpuRenderer` 在 present 前写 destination，与 WSI 的 attach/commit **同一次**生效；UI 线程**不** commit `wl_surface`（唯一例外：建窗的首个 commit 用来取得首个 configure），也不再调 `set_buffer_scale(1)`（fractional-scale 要求 buffer scale 恰为 1，默认即 1）；整数回退的 `set_buffer_scale(N)` 同样由 GPU 线程在 present 前写（§15.13）。旧的 `sync_viewport` + 单独 `commit()`（未 attach 新 buffer）已删除。
5. **buffer 尺寸按 fractional-scale 协议取 `round(logical × scale)`**：不是 covering 的 ceil；1.25 / 1.75 下多 1 px 会让整面被合成器以 0.999× 重采样（D3）。`viewport_destination` 保持 `ceil(logical)`；整数回退时 scale 是整数 `N`，同一公式得到 `logical × N`（§15.13）。
6. **合成器节流优先**：即使 GPU 线程阻塞在 `acquire`，UI 线程也不受影响；帧回调让 UI 只在合成器需要时产帧。

### 7.6 present mode：`MAILBOX`

- Wayland 表面**必须**报告 `VK_PRESENT_MODE_MAILBOX_KHR`（规范原文：`vkGetPhysicalDeviceSurfacePresentModesKHR` 对 Wayland surface 必须返回 MAILBOX；[WSI 规范](https://github.com/KhronosGroup/Vulkan-Docs/raw/main/chapters/VK_KHR_surface/wsi.adoc)）。Mesa 的 Wayland WSI 也实现了它。
- 对本引擎的语义正好匹配：最新帧替换待呈现帧，不排队陈旧帧（与 §4.2 的「最新帧槽」同构）。
- `FIFO` 不用：在 Wayland 上 FIFO 会让 `acquire` 阻塞在合成器的 frame callback 上（Mesa 的 `legacy_fifo` 路径），延迟更高，且没有额外收益（MAILBOX 也不撕裂）。
- `incremental_present` 继续作为 `pNext` hint 传入（现状），不依赖它做正确性。

### 7.7 错误与设备丢失

- `VK_ERROR_DEVICE_LOST` ⇒ 粘性 `EngineError::DeviceLost`；`OUT_OF_DATE/SURFACE_LOST` 按 §7.4 重建；`SUBOPTIMAL` 只标记重建。
- `vkQueueSubmit`/`vkQueuePresentKHR` 失败一律先分类 device-lost，再落到 `EngineError::Swapchain`（沿用现状）。
- 关闭：UI 线程 signal shutdown 后**直接 `join`**（不再 2 s 超时后 detach；§6.7 的「UI 线程允许在关闭时等待」同样适用，F-169）。GPU 线程 `vkDeviceWaitIdle`（允许，退出路径）→ 销毁 swapchain/persist/命令池 → 退出。`acquire` 的 100 ms 上限保证遮挡/合成器停摆时 join 有界（最坏多等一个超时周期）。`join` 返回即 teardown 完成（`GpuRenderer` 在通知之前显式 drop）。
- panic 出路：GPU 线程展开时 `PanicGuard` 把 `EngineError::AppInit { detail: "kytrix-vulkan thread panicked" }` 发布到共享错误槽（不覆盖循环已发布的错误，并置 `terminated`），平台泵的 `poll()` 因此看到错误而不是静默停摆（F-170）。

---

## 8. 对照、阻塞点与被否决方案

### 8.1 原生机制对照（评审时按这张表核对「没有自造轮子」）

| 需求 | macOS | Windows | Linux |
|---|---|---|---|
| 提交异步 | `MTLCommandBuffer.commit` | `ExecuteCommandLists`（异步，返回 void） | `vkQueueSubmit` |
| 完成通知 | `addCompletedHandler` | `ID3D12Fence::SetEventOnCompletion` / 槽 fence | `VkFence` / timeline |
| 节奏 | `CAMetalDisplayLink` | `FRAME_LATENCY_WAITABLE_OBJECT` | `vkAcquireNextImageKHR` + 合成器 |
| 背压 | 3 drawable + semaphore(3) | `SetMaximumFrameLatency(1)` + waitable（`SCALING_NONE`，F-185） | image 数 + fence |
| resize | 重建 persist（无等待） | `ResizeBuffers`（GPU 线程） | `oldSwapchain`（无 wait idle） |
| 延迟释放 | completion handler → retire | fence 值 → retire | timeline 值 → retire |
| 局部更新 | persist + scissor + 整面 blit | persist + scissor + dirty rect copy + `Present1` | persist + scissor + 整面 copy（+ incremental hint） |

### 8.2 剩余的「会阻塞」调用（全部在 GPU 线程）

| 平台 | 调用 | 上界 | 为什么可接受 |
|---|---|---|---|
| Metal | `dispatch_semaphore_wait(NOW)` | 0（失败即跳帧） | 不阻塞 |
| D3D12 | `WaitForMultipleObjects(work, completion)`（空闲）/ `(waitable, completion)`（渲染前） | 一个 vsync | GPU 线程就是呈现线程，这是它该等的地方；completion 分支只用来及时发布反馈 |
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
| `render/windows/swapchain.rs` | 加 `IDXGISwapChain2`、waitable、`SetMaximumFrameLatency(1)`、`DXGI_SCALING_NONE` + `SetBackgroundColor`（创建后 / `ResizeBuffers` 后，S-139）、`present1`、`resize`（带 flag）、`waitable()` |
| `render/windows/compositor.rs` | 拆出 `GpuRenderer`（GPU 线程独占）；窗口循环 `gpu_loop` 落在 `render/windows/renderer.rs`（F-167）；`resize` 改由 GPU 线程调 |
| `render/linux/swapchain.rs` | 每 image 一个 `render_finished`；`create` 带 `old`；新增 `recreate`/`retire`；present mode 改 `MAILBOX` |
| `render/linux/compositor.rs` | 拆出 `GpuRenderer`（`gpu_loop` 落在 `render/linux/renderer.rs`，F-167）；`resize` 不再 `wait_idle`；提交去掉 `wait_for_fences`；retire 改 timeline 非阻塞推进；持有 `WpViewport` + `last_destination`，present 前写 destination（F-190）；`set_clear_color` / 初始 `clear_color` = `background_linear`（F-192） |
| `render/linux/renderer.rs` **新增** | 门面（同 Windows）；`new_for_surface(..., background, viewport)`；`Shared`/`State` 背景槽（F-192） |
| `platform/linux/surface.rs` | 删除 `sync_viewport` / `viewport_applied` / `set_buffer_scale(1)` / IME 两处 `wl_surface.commit()`；新增 `take_viewport()`（D1 / D4） |
| `platform/linux/event_loop.rs` | 删 `sync_viewport()` 调用；每轮 configure / scale 变化后 `window.set_surface(physical_size(...))`（round）；建渲染器传 `background` 与 `take_viewport()`（F-190 / F-191 / F-192） |
| `render/macos/renderer.rs` | 重写：display link 委托 + 最新帧/最新 drawable 槽 + GPU 循环；删除 mailbox 的 `requeue/wait_retry/poll_completed` |
| `render/macos/queue.rs` | 删除 `Submitted` 轮询与 `waitUntilCompleted` 背压；`begin` 只取槽；`submit` 只 commit；完成走 `addCompletedHandler` |
| `render/macos/swapchain.rs` | 删除 `next_drawable`；加 display link 相关（或移到 `renderer.rs`）；`contentsGravity` / `backgroundColor` / `contentsScale` / `drawableSize` 的唯一写入口（主线程，F-187 / F-188） |
| `render/macos/compositor.rs` | `submit_payload` 改为「给定 drawable 渲染」；persist 重建不等待；图层属性写接口移到 `renderer.rs`；persist `clear_color` = 背景色 linear（F-188） |
| `render/color.rs` **新增（2026-11-15）** | 共享背景色编码：`background_srgb`（Windows DXGI / macOS `CGColor`）与 `background_linear`（macOS / Linux 的 sRGB 渲染目标 clear），共用 `opaque + ColorResolver`（F-188 / S-141 / S-143） |
| `window.rs` | `submit_gpu` 改为构造 `FrameRequest` 并 `publish`（Linux 填 `surface_local`，F-191）；删除 `renderer.resize`/`is_fresh` 调用；`poll_renderers` 去掉 `cfg`，三平台都调；新增 `surface` / `set_surface` / `frame_rect()`（F-186 / F-189 / F-191）与 `background`（F-184 / F-188 / F-192） |
| `platform/{windows,macos,linux}` 泵 | 每轮调 `poll()`；macOS 每轮调 `sync_pacing`；`WM_SIZE`/configure 只记录尺寸；Windows 还把真实客户区写进 `Window::set_surface`（§6.7）；macOS 在 Dpi / Resized 之后把真实 backing 写进 `Window::set_surface`（§5.3）；Linux 每轮写 `round(logical × scale)` 的 Wayland buffer（F-191） |
| `lib.rs` / `render.rs` | 模块声明更新（新增两个 `renderer.rs`） |

### 9.2 删除（已全部落地：F-164、F-166 … F-182）

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
| live resize 无整帧缩放（Windows，`SCALING_NONE`） | 拖动期间卡片 bbox 恒定（911 帧：725×220，`stretch_w = 1.00000`）、文字/HUD 不缩放且锐度不降；新暴露的右/下区域 = 窗口背景色且 ≤ 2 帧；缩小时右/下裁切、右锚 HUD 滞后 1–2 帧后回位（可接受） |
| live resize 无整帧缩放（macOS，`kCAGravityBottomLeft` + 真实 backing） | stale 帧卡片 bbox = 静止 bbox（1108×350，1.000×/1.000×）并贴**视觉左上**；卡片文字锐度不降（Laplacian 808 vs 800，+1%；旧 HEAD 624，-22%）；新暴露的右/下条带 = 窗口背景色（(13,15,19) ≈ `#0B0E13`，对静止背景 Δ≤1，无黑/白条）；缩小时右/下裁切、放大时新暴露条带 ≤ 1–2 帧；连续 sweep 卡片高 174–175 恒定；最小化 3 s 恢复 1:1、Esc exit 0、stderr 空。**1x / 真实鼠标拖拽未测**（V-121） |
| live resize 无整帧缩放（Linux，destination 与 buffer 原子） | `WAYLAND_DEBUG`：1218 次 destination 变更 **1218 次与 attach 同一次 commit**（0 次无 attach、0 mismatch、0 次 `set_buffer_scale`；旧 HEAD 273/294 次无 attach）；buffer = `round(logical×scale)`（701×501@1.25 ⇒ 876×626，旧 877×627）；卡片 bbox 恒定（1.25：725×220；1.0：580×176）、文字锐度不降（Laplacian 1.25：1715.34；1.0：2019.30；旧 HEAD 1.25：616–1048）；2802 次 resize / 3583 次 present、FrameGate 最大间隔 79.6 ms、exit 0、validation 0 VUID；缩小时右/下裁切（1.0：576×176 / 580×134）。1.25 下 7/70 帧整窗变软（bbox 不变，归因 sway 分数缩放采样）；**Mutter/KWin、真实指针拖拽未测**（V-122） |
| 窗口背景色（`#0B0E13`） | Windows：resize 条带 100% 像素 = `(11,14,19)`（无黑带/二次编码；200 次 resize 后仍一致）；`set_background_color` 在下一次 `Present1` 生效；`GetBackgroundColor` 在**重设后**回读 == 设定（隐藏窗口测试；未做「不重设」对照）。macOS：条带 (13,15,19)/(14,15,19)（F-188）。Linux：clear 夹具 `#0B0E13`→(11,14,19)、`#402080`→(64,32,128)（linear clear → sRGB 往返正确；demo 根节点铺满，帧内不可见）；`set_background_color` 经 `Shared` 槽在 GPU 线程空闲 / 下一帧录制前应用（F-192 / S-143） |
| resize 到 0 宽/最小化/还原 | 不崩溃、不 device lost；恢复后首帧正确（全量）；最小化期间不重建 1×1（`IsIconic` 跳过 `set_surface`），5 s 恢复像素差 0.067% |
| 真实 DPI 150% / 175% | **未实测**（V-120）：本轮实机只跑 125%（DPI=120）；175% 只有 `256.8 × 1.75 = 449.4` 的取整事实（covering 到 450） |
| 空闲 2s | 0 帧；两次截图逐字节相同；UI 线程 CPU 0.0%；GPU 线程无自旋唤醒 |
| 连续帧（HUD/动画）60s | 帧率由显示器决定；HUD 的 FPS 是**提交率**（本机 60Hz 实测 63.9/64.0/60.6，旧版本同环境 61.6–64.4，非回归；与旧口径 58–60 不一致 ⇒ V-120）；无累积延迟（Latency 1.88–13.23 ms） |
| 全屏转场（macOS）/ 遮挡（Win/Linux） | 不阻塞 UI；恢复后继续出帧 |
| 设备丢失注入（D3D12 debug layer / `VK_EXT_device_fault` / Metal 错误） | UI 线程收到粘性错误并退出，不 panic |
| 多窗口 | 各窗口独立节奏；一个窗口 resize 不改变另一个窗口帧率 |
| 关窗 | GPU 线程 join 成功；无 use-after-free（Metal validation / D3D12 debug layer / Vulkan validation 全开） |

### 10.3 测量方法

- 每平台用 `tracing`/`Instant` 记录：`publish` 耗时、GPU 线程 `record/execute/present` 耗时、完成通知延迟；HUD 增加「publish→present」一栏。
- `sample`（macOS）/ WPA（Windows）/ `perf`（Linux）确认等待只出现在 GPU 线程。

---

## 11. 探针与验证项（P0 三项 + Windows / macOS / Linux resize 追加）

| # | 假设 | 实验 | 不成立时的结论 |
|---|---|---|---|
| 1 | `CAMetalDisplayLink` 在主 run loop **common modes** 下，live resize / 全屏转场期间仍回调；暂停/恢复 `isPaused` 不丢 drawable | 200 行探针窗口：拖动缩放 10s + 全屏进出各 5 次，统计回调次数与 drawable 尺寸变化 | 若 live resize 期间不回调：把 display link 同时挂 `.eventTracking`（同 run loop 多 mode）再测；仍不回调则本设计的 macOS 段需要重做（评审决定），**不预置运行期分支** |
| 2 | `ResizeBuffers` 后 `GetFrameLatencyWaitableObject` 返回的句柄仍有效（或返回新句柄），`SetMaximumFrameLatency` 不需重设 | 连续 resize 200 次，每次 resize 后记录句柄值与 `GetMaximumFrameLatency`，并 present 一帧 | 若句柄失效：每次 resize 后重取并比较，不同则关闭旧的（已写进 §6.4 伪代码） |
| 3 | GPU 线程 + calloop 并发操作同一 `wl_display`（WSI 私有队列）稳定 | 探针：GPU 线程 present 10k 帧 + UI 线程同时拖动 resize 200 次；`WAYLAND_DEBUG=1` 找协议错误；用 `perf` 看 UI 线程是否自旋 | 若出现自旋/协议错误：`acquire` 改 `timeout=0`、由 frame callback 驱动重试（引入跳帧重试状态机），需评审 |
| 4 | `SetBackgroundColor` 的 `DXGI_RGBA` 按 linear 还是 sRGB 编码解释 | 同一 HWND swapchain（`B8G8R8A8_UNORM` + `FLIP_SEQUENTIAL` + `SCALING_NONE`）分别传 linear 与 sRGB 编码的主题色，截取 resize 条带像素 | **已实测**：按 sRGB 编码字节直写。linear 值 ⇒ 条带 `(1,1,2)`；sRGB 编码 ⇒ `(11,14,19)`（= `#0B0E13`，229 个新尺寸帧 100% 像素）。官方 “recommend linear” 在本路径不适用 ⇒ S-139 |
| 5 | `ResizeBuffers` 是否保留背景色 | 创建后设置、`ResizeBuffers` 后 `GetBackgroundColor` 回读 | 官方未承诺；隐藏窗口测试在**重设后**回读 == 设定，无法区分 DXGI 是否保留（未做不重设对照）。实现不依赖：`Swapchain::resize` 后无条件重设（S-139 / V-120） |
| 6 | `SetMaximumFrameLatency` 的默认值口径 | 读 Set/Get 两份官方文档；创建后 `GetMaximumFrameLatency()` 实机断言 | `IDXGISwapChain2` 的 Set/Get 两页当前都写默认 1（`IDXGIDevice1::SetMaximumFrameLatency` 才是 3；此前记录的「Set 3 / Get 1」冲突未复现）；实现显式 `SetMaximumFrameLatency(1)`，实机 `Get == 1`（F-185 / V-120） |
| 7 | `contentsGravity` 在 **flipped** `NSView` 下哪个常量才是视觉左上 | 同一窗口、同一 resize 序列分别用 `kCAGravityTopLeft` 与 `kCAGravityBottomLeft`，录屏逐帧看旧内容贴在哪条边、条带出现在哪侧 | **已实测**：`TopLeft` 把旧内容贴左下（条带在上/右），`BottomLeft` 才贴视觉左上（条带在右/下）。结论写进 `LIVE_RESIZE_GRAVITY` 注释与 S-142；不要按「TopLeft = 左上」的字面直觉改 |
| 8 | macOS 条带颜色与页面背景的 8-bit 差（layer sRGB + clear linear） | resize 帧截取窗口右/下条带与静止页面的同一像素区域，比较 RGB | **已实测**：条带 (13,15,19)/(14,15,19)，静止背景 (12,14,18)，Δ≤1（≈ `#0B0E13`，无黑/白条、无二次编码）。两种编码不能互换（S-141） |
| 9 | drawable 池重建是否掉帧 / 是否出现拉伸帧 | live resize 连续 sweep 录屏，逐帧量卡片 bbox 与高度 | 录屏 54–58 fps（录屏口径，不是引擎 FPS）；卡片高 174–175 恒定、无拉伸帧；`Skip` 只跳 vsync 不产生拉伸。GPU 侧未单独统计掉帧（HUD FPS 是提交率，不代表呈现） |
| 10 | display link 在 AppKit 的 resize 期间是否仍回调（common modes） | AX 连续 `setSize`（约 28 ms/步）+ 单次跳变 960×778 → 1400×950，统计每个尺寸是否有新 drawable 被消费 | **已实测**：resize 期间 display link 持续给 drawable（逐档 resize 的 stale 帧只跳一个 vsync，下一次取到的 drawable 就匹配新尺寸）；**真实鼠标拖拽**（`NSEventTrackingRunLoopMode`）与 1x 未验（V-121） |
| 11 | Linux `wp_viewport.set_destination` 与 buffer 能否在**同一次** present 提交里原子生效 | `WAYLAND_DEBUG=1` 统计每次 destination 变更前后是否有 attach + commit；旧 / 新代码各跑 1.0 / 1.25 的 jump + sweep | **已实测**：旧 HEAD `8598fe3` 273/294 次 destination 变更无 attach（旧 buffer 被缩放）；新代码 1218/1218 与 attach 同一次 commit（0 无 attach、0 mismatch、0 `set_buffer_scale`）⇒ F-190 |
| 12 | fractional-scale 的 buffer 尺寸是 `round` 还是 `ceil` | 1.25 下 701×501 逻辑：比较 buffer 实际尺寸与 `round(701×1.25)` / `ceil(...)`，并量文字 Laplacian | **已实测**：旧 ceil 877×627（整面 0.999× 重采样），新 round 876×626；卡片 bbox 恒定、Laplacian 1715.34（旧 616–1048）⇒ F-191 |
| 13 | Vulkan sRGB attachment 的 clear 值按 linear 还是 sRGB 字节解释 | 无根节点的 `/tmp` 临时夹具分别 clear `#0B0E13` 与 `#402080`，`grim` 后读像素 | **已实测**：`(11,14,19)` / `(64,32,128)`，linear clear → sRGB 往返正确（与 Windows S-139 的 sRGB 字节直写相反、与 macOS S-141 的 clear linear 一致）⇒ S-143 |
| 14 | Linux resize 期间是否有 `OUT_OF_DATE` 重试、Mutter / KWin 的行为是否与 sway 一致 | 逐次统计 `vkQueuePresentKHR` / `OUT_OF_DATE`；在 Mutter / KWin 上重复同一 resize 序列 | **未验**（V-122）：只做了 headless sway 1.11 / wlroots 0.19 / lavapipe；`OUT_OF_DATE` 未逐次计数、Mutter / KWin 未跑 |

前三项是设计期的 P0 探针（只需要一次性探针代码，不进主干）；4–6 是 2026-11-15 Windows
resize 修复时新增的验证项（未闭环部分登记 V-120）；7–10 是同一天 macOS live-resize 修复新增的
验证项（结论见 §5.3 / §5.4 / §15.10，SDK 事实登记 S-142，未验证项登记 V-121）；11–14 是同一轮
Linux live-resize 修复新增的验证项（结论见 §7.3 / §7.5 / §15.11，协议事实登记 S-144，未验证项
登记 V-122）。

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
- [CAMetalLayer](https://developer.apple.com/documentation/quartzcore/cametallayer/)、[drawableSize](https://developer.apple.com/documentation/quartzcore/cametallayer/drawablesize)、[nextDrawable](https://developer.apple.com/documentation/quartzcore/cametallayer/nextdrawable/)、[maximumDrawableCount](https://developer.apple.com/documentation/quartzcore/cametallayer/maximumdrawablecount)
- [CALayer.contentsGravity](https://developer.apple.com/documentation/quartzcore/calayer/contentsgravity)（默认 `resize`）、[kCAGravityBottomLeft](https://developer.apple.com/documentation/quartzcore/calayercontentsgravity/bottomleft)、[contentsAreFlipped()](<https://developer.apple.com/documentation/quartzcore/calayer/contentsareflipped()>)、[isOpaque](https://developer.apple.com/documentation/quartzcore/calayer/isopaque)、[backgroundColor](https://developer.apple.com/documentation/quartzcore/calayer/backgroundcolor)、[contentsScale](https://developer.apple.com/documentation/quartzcore/calayer/contentsscale)
- [NSView.convertSizeToBacking: / convertToBacking(_:)](https://developer.apple.com/documentation/appkit/nsview/converttobacking(_:)-4ra9y)、[NSView.isFlipped](https://developer.apple.com/documentation/appkit/nsview/isflipped)
- [CGColor](https://developer.apple.com/documentation/coregraphics/cgcolor)、[CGColor(srgbRed:green:blue:alpha:)](https://developer.apple.com/documentation/coregraphics/cgcolor/init(srgbred:green:blue:alpha:))
- [Synchronizing CPU and GPU work](https://developer.apple.com/documentation/metal/synchronizing-cpu-and-gpu-work)、[Triple Buffering](https://developer.apple.com/library/archive/documentation/3DDrawing/Conceptual/MTLBestPracticesGuide/TripleBuffering.html)
- [MTLCommandBuffer addCompletedHandler](https://developer.apple.com/documentation/metal/mtlcommandbuffer/addcompletedhandler(_:))、[presentDrawable](https://developer.apple.com/documentation/metal/mtlcommandbuffer/presentdrawable(_:))

D3D12 / DXGI：
- [DXGI flip model](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/dxgi-flip-model)、[DXGI_SWAP_CHAIN_FLAG](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/ne-dxgi-dxgi_swap_chain_flag)、[DXGI_SCALING](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/ne-dxgi1_2-dxgi_scaling)
- [IDXGISwapChain1::SetBackgroundColor](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgiswapchain1-setbackgroundcolor)、[IDXGISwapChain1::GetBackgroundColor](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgiswapchain1-getbackgroundcolor)
- [GetFrameLatencyWaitableObject](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_3/nf-dxgi1_3-idxgiswapchain2-getframelatencywaitableobject)、[SetMaximumFrameLatency](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_3/nf-dxgi1_3-idxgiswapchain2-setmaximumframelatency)、[GetMaximumFrameLatency](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_3/nf-dxgi1_3-idxgiswapchain2-getmaximumframelatency)
- [Reduce latency with DXGI 1.3 swap chains](https://learn.microsoft.com/en-us/windows/uwp/gaming/reduce-latency-with-dxgi-1-3-swap-chains)
- [ResizeBuffers](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/nf-dxgi-idxgiswapchain-resizebuffers)、[DXGI best practices](https://learn.microsoft.com/en-us/windows/win32/direct3darticles/dxgi-best-practices)
- [ID3D12CommandAllocator::Reset](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12commandallocator-reset)、[ExecuteCommandLists](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12commandqueue-executecommandlists)
- [D3D12HelloFrameBuffering](https://github.com/microsoft/DirectX-Graphics-Samples/blob/master/Samples/Desktop/D3D12HelloWorld/src/HelloFrameBuffering/D3D12HelloFrameBuffering.cpp)

Vulkan / Wayland：
- [Frames in flight](https://docs.vulkan.org/tutorial/latest/03_Drawing_a_triangle/03_Drawing/03_Frames_in_flight.html)、[vkAcquireNextImageKHR](https://registry.khronos.org/vulkan/specs/latest/man/html/vkAcquireNextImageKHR.html)、[VkPresentModeKHR](https://docs.vulkan.org/refpages/latest/refpages/source/VkPresentModeKHR.html)、[vkQueuePresentKHR](https://registry.khronos.org/vulkan/specs/latest/man/html/vkQueuePresentKHR.html)
- [VkSwapchainCreateInfoKHR (oldSwapchain)](https://registry.khronos.org/vulkan/specs/latest/man/html/VkSwapchainCreateInfoKHR.html)、[vkDestroySwapchainKHR](https://docs.vulkan.org/refpages/latest/refpages/source/vkDestroySwapchainKHR.html)
- [Vulkan WSI: Wayland Platform](https://github.com/KhronosGroup/Vulkan-Docs/raw/main/chapters/VK_KHR_surface/wsi.adoc)、[swapchain_recreation sample](https://docs.vulkan.org/samples/latest/samples/api/swapchain_recreation/README.html)
- [VK_KHR_timeline_semaphore](https://docs.vulkan.org/refpages/latest/refpages/source/VK_KHR_timeline_semaphore.html)
- [viewporter](https://wayland.app/protocols/viewporter)（`wp_viewport.set_destination` 改变 surface 尺寸并缩放 buffer）、[fractional-scale-v1](https://wayland.app/protocols/fractional-scale-v1)（buffer = `round(logical × scale)`、`buffer_scale = 1`）、[xdg-shell](https://wayland.app/protocols/xdg-shell)（configure 尺寸是 surface-local 逻辑尺寸）、[wl_surface](https://wayland.app/protocols/wayland#wl_surface)（`commit` / `frame` 回调语义）

---

## 15. 审查修订记录（2026-11-14）

实现落地后的对抗性审查发现的问题与修订。**只记录已经落进代码/文档的部分**；第一轮
（F-166 … F-175）见 §15.1–§15.4，第二轮（F-176 … F-182）与三平台当前实测数字见
§15.5–§15.6；F-183 复审、Windows live-resize（F-184 … F-186 / S-139 / S-140）、macOS
live-resize（F-187 … F-189 / S-141 / S-142）与 Linux live-resize（F-190 … F-192 /
S-143 / S-144 / V-122）见 §15.7–§15.11。

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
| Windows 反馈发布点（P2） | `FrameFeedback` 改为 GPU 线程**观察到 fence 完成**时发布：提交时把 `{index, stats, CPU samples, published_at, signal}` 入 pending 队列，`SetEventOnCompletion` + `WaitForMultipleObjects([work_event, completion_event])` 明确唤醒（不轮询）；完成观测点按提交顺序发布、在同一处 resolve GPU 采样并推进 retire。`latency = completed_at - published_at`，两者同源（§4.1 / §6.5） |
| Windows resize 显式失败（P2，原 P1 症状） | `GpuRenderer::resize` 每一步都 `classify()`；GPU 循环收到任何 resize 错误都 `publish_error` + 粘性错误 + 退出线程，不再 `continue` 丢帧（§6.6） |
| Windows 关闭握手（P2） | 删除 `JOIN_TIMEOUT` 与 2 s detach：`MsgWaitForMultipleObjects(... INFINITE ...)` 边泵消息边等 GPU 线程句柄（仅 `WAIT_FAILED` 记录后放弃，§6.7 / F-169） |
| Windows 图集所有权与校验（P2） | `GpuRenderer::queue_atlas_upload` / `AtlasTextures::queue_upload` 改收 `Vec<u8>` 并**按所有权移动**（去掉槽 → 纹理的第二次 16 MiB 拷贝）；整页尺寸校验 `debug_assert_eq!` → 始终生效的 `assert_eq!`（release 下不再越界读） |
| Windows GPU 线程断言（P2） | 窗口路径入口补 `debug_assert!(on_gpu_thread())`（`waitable` / `queue_atlas_upload` / `acquire_slot` / `wait_slot_fence` / `set_completion_event` / `completed_value` / `resolve_frame_timer` / `release_completed`）；离屏路径不加 |
| Windows `Swapchain::size`（P2） | 只写不读的死字段删除 |
| 共享层 `cfg(windows)` 清零（P2） | `FramePlan::copy_plan` 移到 `render/windows/compositor.rs`（等价单测随迁）；`InFlightRing::reserve` 删除，`Queue::submit_immediate` 改用槽 0 的真实 fence 记账 |
| Windows resize 测试 fixture（P2） | `minimum_size_clamps_the_dragged_edge` 的抓取点改为左边缘 `(100, 300)`（`session()` 显式收 cursor）；生产 clamp 逻辑未改 |

### 15.2 仍未闭环

> 本节逐条注明「已知、未修、原因」。这些都是**已登记**的边界，不是待办清单：修它们需要新的
> 夹具 / 设备 / 功能落点，本轮只保证不把未验证的东西写成已闭环。

- **live resize 探针**（§11 探针 1）：`CAMetalDisplayLink` 的 common-modes 回调只在普通窗口下
  验过；drag-resize 的连续帧与全屏转场期间的回调节奏没有形成实机结论。**已知、未修**：需要能
  持续拖动窗口的 GUI 夹具（AX 只能离散设尺寸，构造不出 AppKit 的 live-resize run loop），
  本轮没有投入。
- **设备丢失注入**（§10.2）：三平台的错误分类都有单测，但没有 D3D12 debug layer /
  `VK_EXT_device_fault` / Metal 错误注入的实机用例。**已知、未修**：注入需要驱动/调试层支持与
  独立夹具，且会破坏正常工作环境；留作专项验证，不影响本轮「错误粘性冒泡」的代码路径。
- **多窗口**（§10.2）：每窗口一条 GPU 线程的结构已就位，但「一个窗口 resize 不影响另一个窗口
  帧率」未实测。**已知、未修**：demo 是单窗口，需要额外的多窗口夹具与两套 swapchain 采样。
- **120Hz / 全屏转场**（§10.2）：只有 60Hz 内建显示器，120Hz（ProMotion / 外接高刷）未验；
  全屏转场只在 HUD 开/关两条 AX 路径下验过，未在 live 转场期间采样 display link 回调。
  **已知、未修**：硬件与夹具都不具备（本机 Intel Iris Plus 655 只有 60Hz）。
- **Linux snapshot 既有 VUID**：Linux 离屏 snapshot 用例在 validation 层下会报**既有** VUID
  （本轮之前就存在，与窗口异步管线无关）。**已知、未修**：它属于离屏/读回路径的同步用法，本轮
  没有触碰；修它需要单独复核该路径的 barrier/同步并重跑 13 份 golden，因此本轮只把结论限定在
  窗口路径（10 s 0 error / 0 `SYNC-HAZARD`）。

### 15.3 实现修订中的条款（已全部闭环）

原「实现修订中」清单只剩 macOS 关闭握手一项，现已闭环：

- §7.7 的关闭握手：Windows（消息泵无限等，F-169）、Linux（直接 `join`，§15.4）已落实；macOS
  也已删除 `recv_timeout` + detach 与 `ExitSignal`，`Drop` 直接 `join`，等待由
  `SHUTDOWN_GRACE = 2 s` 兜底（从 `request_shutdown` 起累计，F-169 / F-179）。**本清单已空**，
  不再有「实现修订中」的条目。

### 15.4 Linux 修订记录

Linux/Vulkan 后端的对抗性审查修复（`fix(linux): wait acquire at TRANSFER, join on shutdown,
publish feedback on timeline completion`）。实机口径：Ubuntu 26.04 / lavapipe / headless sway
（`WLR_BACKENDS=headless` + `WLR_RENDERER=pixman`），demo + HUD 10 s。

| 项 | 修订 |
|---|---|
| **P0 acquire 信号量的 wait stage**（F-168） | `image_available[slot]` 的 `wait_dst_stage_mask` 改为 `TRANSFER`；`copy_to_swapchain` 的 `UNDEFINED → TRANSFER_DST` barrier 的 `srcStageMask` 由 `TOP_OF_PIPE` 改为 `TRANSFER`（dst 已是 `TRANSFER`）。本实现不直接往 swapchain image 上画，首次访问就是 transition + 整面 copy，因此只留 `TRANSFER`。修复前 syncval 稳定报 4 条 `SYNC-HAZARD-WRITE-AFTER-READ`（`vkCmdPipelineBarrier` 与 `vkAcquireNextImageKHR` 无依赖链），修复后 syncval 10 s **0 条**。`incremental_present` 的 `pNext` 不受影响（§7.3） |
| **P1 关闭握手**（F-169） | 删除 `ExitSignal`、`JOIN_TIMEOUT`、`recv_timeout(2s)` + detach：`WindowRenderer::drop` 只做 `signal shutdown → thread.join()`（无超时）。`gpu_loop` 持有局部 `Option<GpuRenderer>`，循环结束显式 `drop(renderer.take())` 之后线程才返回，因此 `join` 返回 = teardown（`vkDeviceWaitIdle` / swapchain / persist / device）已完成。实机 `swaymsg kill` 与 `wtype -k Escape` 均 exit 0、关闭 109–116 ms，无 join 卡住 |
| **P1 panic 出路**（F-170） | 新增 `PanicGuard`：`gpu_loop` 展开时（`std::thread::panicking()`）向共享错误槽发布 `EngineError::AppInit { detail: "kytrix-vulkan thread panicked" }` 并置 `terminated`，`poll()` 不会静默停摆；正常退出 `disarm()`，不覆盖循环已发布的错误，`publish_panic` 也不覆盖已有错误（3 条单测） |
| **P2 反馈改到完成时** | 提交时把 `{index, stats, CPU samples, published_at, timeline_value(signal), slot}` 放进 pending 队列；每轮循环用 `vkGetSemaphoreCounterValue` 非阻塞推进，观察到完成之后按提交顺序发布 `FrameFeedback { index, stats, samples(+此处 resolve 的 GPU 采样), completed_at, latency }`，retire 在同一观测点推进。GPU 采样不再在提交点读（避免读在飞槽），只在完成之后 resolve。等待策略：pending 非空时 `Condvar::wait_timeout(4ms)`，其余无限等待（0 自旋）——避免最后一帧的反馈永远不发布 |
| **P2 samples 断言** | `feedback.samples.len() <= 4` → `<= 3`（Upload / Gpu / Present，`FrameTimings` 只有这三段） |
| **P2 atlas staging 只有一份** | 本次不改：`frame_ready` 门禁把在飞帧压到 ≤ 1（UI 只在上一帧的 `wl_callback.done` 之后发布下一帧，而该回调意味着合成器已取走上一帧 image、其 `render_finished` 依赖的 copy 必然完成），CPU 写 staging 与 GPU 读 staging 不会重叠。代码与 §7.3 已显式登记该设计假设：**在飞 ≤ 1**；放开在飞帧数时必须按帧槽拆分 staging 或先等该帧 fence。**（F-183 复审后已改为每次上传一次性 staging，见 §15.8：活性兜底本身就会放行第二帧，staging 不再依赖「在飞 ≤ 1」。）** |
| 验证 | `cargo check -p ui_core --all-targets` 与 `--features hud,test-support` 全绿；lib tests **632 passed / 0 failed**（该修订落地时；当前 641，见 §15.6 / §15.8）；snapshot 8/8；hud 7/7；`cargo +nightly clippy -p ui_core --all-targets -- -D warnings` 全绿；`cargo +nightly fmt --check` 全绿；实机普通 validation 10 s **0 error / 0 warning**；syncval 10 s **0 SYNC-HAZARD**（评审复现用的 `VK_LAYER_ENABLES=...SYNCHRONIZATION_VALIDATION_EXT` 旧写法只剩 `VALIDATION-SETTINGS` 的弃用提示，不是 API 用法告警；改用 `VK_LAYER_VALIDATE_SYNC=1` 时日志完全为空）；resize 1280×720 → 900×600 → 640×480 正常（HUD `640x480 @ 1.00x Vulkan`）；HUD `Latency` 在 2 s / 6 s / 10 s 为 1.80 / 1.96 / 1.89 ms（不累积；当轮数值，当前 1.68–1.76 ms 见 §15.6 / §15.8）；关闭见上 |

### 15.5 第二轮闭环（F-176 … F-182，2026-11-14）

macOS 异步回归的 P2（`8c02aa0`）、Linux 反馈判据与 Windows panic/关闭例外（`d6f56fb`）、关闭
grace 计时（`69830d2`）与完成通知门禁测试（`68758de`）已全部落地；偏差与原因见
[`DEVIATIONS.md`](DEVIATIONS.md) §29。

| 项 | 修订 | 落点 |
|---|---|---|
| 槽位轮转口径（F-176） | `InFlightRing::acquire` 按 **acquire 次数**轮转；Linux 的 `NOT_READY` / `OUT_OF_DATE` 重试会多消费一次 acquire 而不提交，F-171 的「提交次数」措辞据此更正 | `render/frame.rs`（+ `acquire_rotates_slots_by_acquisition_count`）、Linux `Device::slot` |
| Linux 完成判据（F-177） | 反馈完成判据改为 `submit` 返回的 timeline 值（非 ring 计数）：帧序号 1/4/7 跳号时不再误发布在飞帧 | `render/linux/renderer.rs`（+ `only_frames_whose_timeline_value_completed_are_published`） |
| 三平台 panic 出路（F-178） | Windows 补 `PanicGuard`：GPU 线程展开也以粘性 `EngineError::AppInit` 回到 `poll()`；三平台同构 | `render/{windows,linux,macos}/renderer.rs` |
| macOS 退化 / scissor / 关闭（F-179） | 粘性退化（每 vsync 按 drawable 尺寸全量，直到尺寸重新匹配；诊断每 episode 一次）；0 面积 scissor 跳过；`SHUTDOWN_GRACE = 2 s` 有界 join（从 shutdown 请求起累计） | `render/macos/renderer.rs`、`render/macos/compositor.rs` |
| macOS 反馈顺序（F-180） | `FeedbackQueue` + `CompletionOrder` 连续前缀：完成回调乱序也按提交顺序发布 | `render/macos/renderer.rs`、`render/macos/queue.rs` |
| 图集所有权（F-181） | 三平台 GPU 侧 `queue_atlas_upload` / `queue_upload` 收 `Vec<u8>` 并按所有权移动，去掉槽 → 纹理的第二次 16 MiB 拷贝 | `render/*/compositor.rs`、`render/*/atlas.rs` |
| Windows 关闭例外（F-182） | 关闭时允许「waitable 已消费但不 present」（swapchain 随即销毁）；token 守恒的其他两个不画出口仍是断言 | `render/windows/renderer.rs`、§6.3 |

### 15.6 三平台当前实测数字（2026-11-14）

| 平台 | lib 单测 | HUD `Latency`（demo + HUD） | 说明 |
|---|---|---|---|
| Windows | 695 passed / 0 failed / 1 ignored | 7.8–10.2 ms | 跨 10 s 多次采样，不累积；其中大部分是 waitable 的 vsync 排队（I3 的既有语义） |
| Linux | 641 passed / 0 failed | 1.68–2.33 ms | headless sway + lavapipe，1276×678 窗口；2/6/10 s 采样不累积（2026-11-15 两轮：1.76/1.68/1.68 与 2.33/1.74/1.83 ms；§15.4 的 2.1–3.1 ms 是上一轮） |
| macOS | 652 passed / 0 failed | 默认 1920×1556：3.35–11.55 ms；1800×1200：2.91–3.59 ms | 同一台 Intel Iris Plus 655；范围随 drawable 像素面积变化，同尺寸跨 10 s 不累积（`b4ea69e` 基线重测 2.90–3.41 ms；此前文档的 648 是旧基线） |

- 测试数字是 `cargo test -p ui_core --lib` 口径（默认 feature）；feature 组合会改变数量（同一
  HEAD 加 `--features "hud,test-support"` 时 Windows 为 756 passed / 1 ignored），比较时只与
  同口径历史值比。整套（含集成）数字见各平台验证文档。
- Linux 的 641 = F-183 的 5 条 `FrameGate` 单测 + 本轮（§15.8）新增的 1 条迟到 callback 单测
  + 1 条图集 staging 单测（`platform/linux/frame_gate.rs` / `render/linux/atlas.rs`，只在
  Linux 构建里编译；Windows / macOS 的数字不受影响）。
- `Latency` 是「UI 发布 → GPU 线程观察到完成」，**同一窗口尺寸下跨 10 s 不增长**；绝对值随
  窗口像素面积、机器、驱动、合成器与负载变化（同一台机不同 run 也可差 1 ms 以上），只作
  同机、同尺寸回归对比，不作跨平台契约。

### 15.7 第三轮闭环（F-183 + 未使用依赖，2026-11-14）

- **Linux `request_frame` 的 Idle 门禁（既有，F-183）**：`FrameGate` 在 `FrameOutcome::Idle`
  与中止帧（`App::frame` 返回 `None`）时立即重开门禁，并**保留**挂起 callback（下一次 commit
  时自然返回）；present `OUT_OF_DATE`（WSI 没有 attach/commit）由 200 ms 活性兜底覆盖，
  `dispatch` 的阻塞上限按剩余兜底时间压缩，避免门禁关闭期间泵睡到 1 h。200 ms 不是呈现节奏，
  `App::frame` 的共享契约不变（没有拆分 encode/commit）。偏差与完整理由见
  [`DEVIATIONS.md`](DEVIATIONS.md) §30。
- **workspace 未使用依赖（P2）**：删除根 `Cargo.toml` 的 `lyon` / `usvg` / `ui_core` 三个没有
  任何成员用 `workspace = true` 引用的声明，以及 `crates/ui_core/Cargo.toml` 的 `lyon` 成员
  依赖；`cargo check` 不再报 `unused dependency` / `unused workspace dependency`。`lyon` /
  `usvg` 仍是第三方 crate（如 `gpui-pre`）的传递依赖，所以 `Cargo.lock` 只删掉 `ui_core`
  条目里的 `lyon` 一行。

### 15.8 第四轮闭环（F-183 复审：迟到 callback + 一次性 staging，2026-11-15）

最终复审在 Linux 发现两个仍会让**两帧同时在飞**的 P1。活性兜底本身就会在 present 卡死时放行
第二帧（这是它存在的目的），因此门禁的「最多一个挂起 callback」与图集 staging 的「在飞 ≤ 1」
都不能只靠恢复门禁来保证。

- **迟到 callback 的归属**（`platform/linux/frame_gate.rs`、`platform/linux/surface.rs`）：旧
  实现用 `requested: bool`——`expire` 清标志后新 `request` 会真的再挂一个 callback，而旧
  callback 迟到时 `on_callback` 无条件清标志并开门，把**新请求**的 pending 清掉，第 3 次
  `request` 又能再挂一个，门禁的「最多一个挂起 callback」被破坏。改为 `CallbackToken`（每次
  `request` 发新令牌）：`expire` 放弃当前令牌，`on_callback(token)` 只接受当前令牌，迟到 /
  重复的旧回调直接忽略。平台层用 `wl_callback` 的 `ObjectId` 把 `done` 事件归属到令牌（令牌
  只跟当前请求走）。没有用计数模型：旧 callback 与新 callback 都只能让计数 -1，无法区分归属
  —— 要么迟到回调消耗新请求的计数，要么 Idle 路径（`note_not_committed` 重开门禁）重新开始
  无限堆积 callback。`note_not_committed` 仍然立即开门并保留挂起的令牌；`ready` 语义不变。
- **图集 staging**（`render/linux/atlas.rs`）：改为**每次上传一块一次性 `VkBuffer`**（只装本次
  脏行），录制时使用，提交后按本帧序号进 `RetireQueue`，timeline 完成推进释放（I4）。选择它
  而不是「按 3 个 in-flight 槽拆 3 份」：per-slot 是 3×整页（字形 4 MiB + 图片 16 MiB →
  60 MiB 常驻），一次性只随在飞帧的脏行增长；上传是低频路径，分配/映射代价可忽略。与 macOS
  本轮的修复同策略，生命周期由完成前沿显式保证，而不是依赖槽 fence 的隐式关系。顺带修正
  离屏 / 单测的 `VkDeviceCreateInfo`：没有 `VK_KHR_surface` 的 headless 实例不再启用依赖它的
  `VK_KHR_swapchain` / `VK_KHR_incremental_present`（validation 的 -01387 消失）。
- **回归**：`a_late_callback_after_the_liveness_expiry_does_not_clear_the_new_request`（旧实现
  实测 FAIL：迟到的旧 callback 之后第 3 次 `request` 又挂了一个；新实现：仍有 1 个挂起、第 3
  次 `request` 返回 `None`）；`consecutive_uploads_do_not_share_staging_while_frames_are_in_flight`
  （两帧先录制后提交，按帧释放各自 staging，读回第 5 / 7 行各自颜色，未上传行保持 0）。
- **验证（2026-11-15，lavapipe / headless sway）**：lib **641 passed / 0 failed**；snapshot
  **8/8**；hud **7/7**；`cargo +nightly clippy -p ui_core --all-targets -- -D warnings` 与
  `cargo +nightly fmt --check` 全绿；`VK_LAYER_VALIDATE_SYNC=1` demo 10 s **0 SYNC-HAZARD**
  （只剩 `VALIDATION-SETTINGS` 的 deprecated-layers 提示）；普通 validation 10 s **0 error /
  0 warning**；resize 640×480 正常（HUD `640x480 @ 1.00x Vulkan`）；`swaymsg kill` exit 0、
  关闭 110–113 ms；HUD `Latency` 两轮 2 s / 6 s / 10 s 采样为 1.76 / 1.68 / 1.68 ms 与
  2.33 / 1.74 / 1.83 ms（同尺寸不累积；run 间浮动 <1 ms，符合「只作同机对比」的口径）。
  Windows **695 passed / 1 ignored**、macOS **652 passed**（两平台的代码未变，仅复核）。
- **workspace 级 clippy 的既有失败（与轮次无关）**：从仓库根跑
  `cargo +nightly clippy --all-targets -- -D warnings` 会在 `crates/network` 的
  `transport/subscription.rs:194/231` 报 2 条 `redundant_clone`（`status()` 已按值返回
  `SubscriptionStatus`，调用点的 `.clone()` 确实多余）；这两行在 `ff03aed` 就存在，本轮的
  `ui_core` 改动不涉及该 crate。`-p ui_core` 的口径全绿。

### 15.9 第五轮闭环（Windows live resize：`SCALING_NONE` + 真实客户区 + 背景色契约，2026-11-15）

本轮只改 Windows 窗口路径；macOS/Linux 渲染行为零变化（S-140）。偏差登记见
[`DEVIATIONS.md`](DEVIATIONS.md) §31（F-184 … F-186 / S-139 / S-140 / V-120）。修复前
（`DXGI_SCALING_STRETCH`）快速拖动 93% 帧 card 被拉伸（峰值 1.10%），300 px 跳变时整帧放大
1.25×（card 907×269、+37 ms）、HUD lap 掉到 38–52%；修复后同一夹具的实测摘要如下（实机环境：
Windows / 硬件 D3D12 / 3840×2160 @125%，窗口 DPI=120，demo client 1200×925，卡 725×220 @ (30,100)）。

| 项 | 结论 |
|---|---|
| swapchain | `FLIP_SEQUENTIAL` + `DXGI_SCALING_NONE` + `SetBackgroundColor`（创建后 / `ResizeBuffers` 后）；3 buffers 与 waitable flag 保留（§6.2 / F-184） |
| 背景色编码 | 本路径按 sRGB 编码字节直写：linear 主题色 ⇒ `(1,1,2)`，sRGB 编码 ⇒ `(11,14,19)`；官方 “recommend linear” 不适用（§6.2 / S-139） |
| 帧延迟 | `SetMaximumFrameLatency(2 → 1)`；隐藏窗口实机 `GetMaximumFrameLatency() == 1`（F-185） |
| surface | Windows = 真实客户区（`run` / `WM_SIZE` / `WM_DPICHANGED` 后 `set_surface`；最小化 `IsIconic` 跳过）；`paint_frame` / `commit_frame` 走 `frame_rect()`（§4.4 / §6.4 / F-186）；**2026-11-15 追加（第八轮 §15.12）**：`submit_gpu` 不再自行调 `frame_rect()`，改消费 `commit_frame` 算好的 `plan`（F-193） |
| 背景色契约 | `WindowOptions.background`（默认 BLACK、不透明、仅 Windows）+ `set_background_color`；GPU 线程两个应用点，下一次 `Present1` 生效（§6.3 / §6.7） |
| 实测（125%） | 300×200 跳变：stale 帧 9.4–11.7 ms、卡片恒 725×220、条带 100% `(11,14,19)`（229 帧 / 3 次跳变）、首原生帧 42.3–46.0 ms、HUD lap 4687.5–4696.3；911 帧拖动 `stretch_w = 1.00000`、HUD lap 4707.5–4936.2（静止 4633.7，无下降）；200 次 resize（fails=0，4.58 s）后条带不变；缩小 1500×1125→1350×1000 首帧裁切、~33 ms 后回位；60.2 s soak Latency 1.88–13.23 ms、FPS 63.9/64.0/60.6；最小化 5 s 恢复像素差 0.067%；Esc 关窗 90 ms / exit 0 / stderr 0 字节；无 `0x887A0001`（§10.2 / §11） |
| 未验证 | 150% / 175% 真实 DPI、最小化 + `WM_DPICHANGED` 组合、`ResizeBuffers` 是否保留背景色、`SetMaximumFrameLatency` 默认值文档口径、HUD FPS 与旧口径 58–60、隐藏窗口测试仍 `#[ignore]`、非 Windows `frame_rect` 0×0 仅编译检查（V-120） |

> **注**：本节是 Windows 轮次的原记录（当时 macOS / Linux 未改）；后续轮次（macOS §15.10 /
> Linux §15.11 / 第八轮 §15.12）已扩展，勿与 HEAD 当前行为混读。表中「仅 Windows」「非 Windows `frame_rect` 0×0」
> 属于当时记录，正文保留。

### 15.10 第六轮闭环（macOS live resize：`kCAGravityBottomLeft` + 真实 backing + 背景色，2026-11-15）

macOS 的 live resize 把旧 drawable 交给 CoreAnimation 缩放：`CAMetalLayer.contentsGravity` 默认
`resize`，AppKit 在 resize 时同步放大 layer bounds，而 `drawableSize` 不会自动跟随；引擎又在
`submit_gpu` 用**布局 covering**（`size × scale`）写 `drawableSize`。结果是 stale 帧被 CA 拉伸
（1.466×/1.229× 非等比）、文字发虚。本轮只改 macOS 窗口路径 + 共享颜色编码；Windows 只把
`background_rgba` 换成共享的 `render/color.rs::background_srgb`（行为不变），Linux 未做等价改造
（`frame_rect` 仍是 covering，resize 期间新区域由合成器决定）。偏差登记见
[`DEVIATIONS.md`](DEVIATIONS.md) §32（F-187 … F-189 / S-141 / S-142 / V-121）。

| 项 | 结论 |
|---|---|
| 根因 | `contentsGravity = resize`（默认）+ AppKit 同步放大 layer bounds + `drawableSize` 不自动跟随 + `submit_gpu` 用布局 covering 写 `drawableSize` ⇒ 旧 drawable 被 CA 缩放 |
| 贴放 | `contentsGravity = kCAGravityBottomLeft`：本视图 `isFlipped = true`，CA 的 `bottomLeft` 才是视觉左上（`TopLeft` 实测把旧内容贴左下、条带在上/右） |
| 真实 backing | `NSView.convertSizeToBacking(view.bounds.size)` 的 round 值（不是 ceil）、至少 1 px；`Window.surface` 与 `drawableSize` 是同一个整数；`sync_state` 的 Dpi / Resized 之后 `set_surface`（先刷新图层、再 early-return）；`submit_gpu` 的 `set_layer_scale` 只作安全网 |
| 背景条 | 两处编码（S-141）：layer `backgroundColor`（sRGB `CGColor`，旧 drawable **之外**）+ persist `clear_color`（**linear**，新 drawable **之内**）+ `NSWindow.backgroundColor`（仅创建期）；`render/color.rs` 共享 `background_srgb` / `background_linear`，Windows 复用前者 |
| F-172 | 不改：surface = 真实 backing 后 requested/drawable 恒等，resize 瞬间的旧 drawable 走 `Skip`（旧内容 1:1 + 图层背景色），`Degraded` 只作活性保底 |
| 实测（2x） | 旧 HEAD `8598fe398` stale 卡片 1624×430（1.466×/1.229×、文字 Laplacian -22%）→ 新代码 1108×350（1.000×/1.000×）、贴左上（y0 = 128/129、h = 174/175，连续 sweep 174–175 恒定）、卡片文字 808 vs 800（+1%）、标题 3272 vs 3279（-0.2%）、条带 (13,15,19)/(14,15,19) 对静止 (12,14,18)（Δ≤1）；最小化 3 s 恢复 1:1、Esc exit 0、stderr 空 |
| 未验证 | 1x 显示模式、真实鼠标拖拽、多显示器 / 混合 DPI、ProMotion、`preferredFrameLatency`、广色域条带接缝 / `layer.colorspace`；Linux 等价改造（V-121） |

- **探针环境与方法**：macOS 15.8.1 / Intel Iris Plus 655 / 内建 2560×1600 @2x；真实窗口 + iTerm2
  会话录屏（54–58 fps，**录屏口径**、不是引擎 FPS）；AX 连续 `setSize`（约 28 ms/步）与单次跳变
  960×778 → 1400×950；窗口 client 960×778（含 38 pt 安全区）⇒ 2x backing 1920×1556。判据：录屏
  逐帧取窗口区域，比较 stale 帧与静止帧的卡片 bbox（倍数）、量卡片文字区域的 Laplacian 方差
  （锐度）、取新暴露条带与静止背景的 RGB。逐帧证据见
  [`macos-verification.md`](macos-verification.md) §2.12。
- **新增 / 迁移单测**：`render/color.rs` 2 条（sRGB 编码那条从 `render/windows/renderer.rs` 迁入，
  linear clear 新增；两平台共享）、`render/macos/swapchain.rs` 1 条
  （`LIVE_RESIZE_GRAVITY` 的字面值 == `bottomLeft`）、`render/macos/renderer.rs` 2 条（待应用背景色
  必须唤醒 GPU 循环；shutdown / terminated 之后丢弃）、`window.rs` 1 条 macOS
  `frame_rect() == 平台 backing`。macOS 专属用例在 `cfg(target_os = "macos")` 下编译。
- **落点**：`render/macos/swapchain.rs`（`new_layer` / `LIVE_RESIZE_GRAVITY` / `set_layer_background`
  / `set_layer_size`）、`render/macos/renderer.rs`（`new_for_layer(background)` /
  `set_background_color` / `Shared::{set,take}_background` / `has_work`）、
  `render/macos/compositor.rs`（`clear_color` / `set_background_color`）、`render/color.rs`（新文件）、
  `platform/macos/window.rs`（`backing_size` / `sync_state` / `NSWindow.backgroundColor`）、
  `platform/macos/event_loop.rs`（建渲染器用真实 backing、首帧前 `set_surface`）、
  `platform/macos/dpi.rs`（删 `physical_size`）、`window.rs`（`surface` / `set_surface` /
  `frame_rect` / `background` 文档与 `cfg(any(windows, target_os = "macos"))`）、
  `render/windows/{renderer,compositor}.rs`（改用 `background_srgb`，删除 `background_rgba`）。

> **注**：本节是 macOS 轮次的原记录（当时 Linux 未改）；Linux 的等价改造由紧接着的 §15.11 闭环
> （F-190 … F-192 / S-143 / S-144 / V-122）。

### 15.11 第七轮闭环（Linux live resize：destination 与 buffer 原子 + 真实 buffer + 背景 clear，2026-11-15）

Wayland 的 live resize 同时踩了四个独立缺陷：`sync_viewport` 把新 destination 单独
`wl_surface.commit()`（不 attach 新 buffer），compositor 立刻按新 destination 缩放**旧** buffer；
UI 线程的 commit 与 GPU 线程 WSI present 的 commit 没有 happens-before；buffer 用 covering 的
`ceil` 而不是 fractional-scale 要求的 `round`；UI 线程的 commit 又会提前触发 `wl_surface.frame`
回调，破坏 FrameGate 的「≤1 在飞」。本轮只改 Linux 窗口路径 + 共享层（`surface` / `frame_rect` /
`background` 的 `cfg` 扩到 Linux 与颜色编码）；Windows / macOS 的已提交行为不变。偏差登记见
[`DEVIATIONS.md`](DEVIATIONS.md) §33（F-190 … F-192 / S-143 / S-144 / V-122），实机证据见
[`linux-verification.md`](linux-verification.md) §4.7。

| 项 | 结论 |
|---|---|
| D1 destination 单独 commit | `platform/linux/surface.rs::sync_viewport` 调 `set_destination` + `set_buffer_scale(1)` + `wl_surface.commit()`（未 attach）⇒ compositor 立即缩放旧 buffer。实测旧 HEAD `8598fe3`：273/294 次 destination 变更无 attach；1.25 静止 buffer 877×627、卡片 bbox 435–774 被拉伸、文字 Laplacian 616–1048，22 ms 后才等到新 buffer |
| D2 跨线程 commit 无序 | UI 线程的 `wl_surface.commit` 与 GPU 线程 WSI 的 present commit 无 happens-before；旧 buffer 可能在 destination 之后 attach ⇒ 修复必须把 destination 移到 GPU 线程、present 之前 |
| D3 buffer 取整 | `physical_size` 用 covering（ceil）；fractional-scale 协议要求 `round(logical × scale)`。1.25 / 1.75 下多 1 px ⇒ 整面被 compositor 以 0.999× 重采样（文字轻微发虚）。新增 `Rect::to_physical_rounded`（half away from zero），`platform/linux/dpi.rs::physical_size` 改 round；`viewport_destination` 保持 `ceil(logical)` |
| D4 frame callback 提前 | UI 线程的 commit（`sync_viewport` + IME 两处）提前触发 `wl_surface.frame` 回调；FrameGate 的 ≤1 在飞前提失效。删除三处 `wl_surface.commit()`（保留建窗初始 commit），IME v1 状态只走 `zwp_text_input_v3.commit` |
| 原子化 | `WaylandState::take_viewport()` 把 `WpViewport` 交给 `Renderer::new_for_surface(..., background, viewport)`；`GpuRenderer` 持有 viewport + `last_destination`，在 `vkQueuePresentKHR` **之前**按 `FrameRequest.surface_local` 写 `set_destination(ceil(logical))`（只在变化时）；present 内部的 attach/commit 让两者**同一次**生效。`Drop` 里 `viewport.destroy()`（surface 仍存活） |
| surface | `Window.surface` = 平台层写入的真实 Wayland buffer：`run` 与每轮 configure / scale 变化后 `window.set_surface(dpi::physical_size(...))`（同值 no-op）；`frame_rect()` 三平台统一为 `surface`（Linux 不再用 covering；`set_surface` 的 0 尺寸 clamp 1） |
| 同帧配对 | `FrameRequest` 加 `#[cfg(target_os = "linux")] surface_local: Size`（`Window::submit_gpu` 填 `self.size()`）；最新帧槽覆盖时 `surface` 与 `surface_local` 一起替换（单测 `replacing_a_frame_keeps_surface_and_surface_local_paired`） |
| 背景色 | `WindowOptions.background` / `set_background_color` 在 Linux 生效：构造时 `background_linear(WindowOptions.background)` 写 persist `clear_color`，运行期经 `Shared` 背景槽（GPU 线程空闲 / 取帧后各应用一次；`wait_for_work` 把待应用背景色也算工作）；clear 按 **linear** 写 Vulkan sRGB attachment（S-143）。`set_background_color` 标 full damage，下一次全量录制生效 |
| 不 seed 帧 | 不改 destination 时旧 buffer 不会被缩放；Wayland 窗口尺寸由客户端 commit 决定，新 buffer 到达时一次跳到新布局。没有 Windows 的「背景条中间态」，因此不引入额外的旧 layout 帧（F-192） |
| 实测（headless sway 1.11 / wlroots 0.19 / lavapipe） | 1.25：701×501 → buffer **876×626**（round；旧 877×627）、destination 701×501，`create_buffer → set_destination → attach → commit` 相邻无中间 commit；卡片 725×220 恒定、静止 Laplacian 1715.34（旧 616–1048）；1.0：卡片 580×176 恒定、Laplacian 2019.30；2802 次 resize / 3583 次 present，**1218 次 destination 变更 1218 次原子**（0 无 attach、0 mismatch、0 `set_buffer_scale`）；FrameGate 最大间隔 79.6 ms；HUD 静止 59.1 fps / 2.13 ms、拖动 39.2 / 6.28 ms；exit 0、validation 0 VUID。1.25 下 7/70 帧整窗变软（bbox 不变；稳定复测 1715.34，归因 sway 分数缩放采样，不是旧 buffer 缩放）；缩小（1.0）右裁 576×176、下裁 580×134 |
| clear 夹具 | `/tmp` 临时夹具（无根节点）：`#0B0E13` → 像素 `(11,14,19)`、`#402080` → `(64,32,128)`，证明 linear clear → sRGB 往返正确；demo 根节点铺满，帧内不可见（F-192 / S-143） |
| 未验证 | Mutter / KWin、真实指针拖拽、逐次 `OUT_OF_DATE` / `vkQueuePresentKHR` 计数、无 viewporter 回退（已在 §15.13 实现 / 验证，V-110 / V-122 ④ 闭环）、1.25 sway 瞬态采样、HUD OCR（`apply_min_size` 已在第八轮修复，见 §15.12） |

- **探针环境与方法**：headless sway 1.11 / wlroots 0.19 / lavapipe，`HEADLESS-1` 1280×720；
  `swaymsg resize set` 做程序化 jump / sweep（没有真实指针拖拽），`grim` 逐帧截图，
  `WAYLAND_DEBUG=1` 统计 commit / attach / destination。判据：每次 destination 变更是否有
  attach 同行、buffer 尺寸是否等于 `round(logical × scale)`、卡片 bbox 是否恒定、卡片文字区域的
  Laplacian 方差（锐度）、缩小时是否裁切、soak 计数与 validation 输出。逐项结果见
  [`linux-verification.md`](linux-verification.md) §4.7。
- **新增单测**：`math.rs` **5 条**（`physical_rounded_buffer_size_table` /
  `physical_rounded_non_integer_product_rounds_not_ceils` / `..._rounds_half_away_from_zero`
  / `..._rounds_the_size_not_the_edges` / `..._non_finite_is_empty`）、`platform/linux/dpi.rs` 2 条
  （`physical_size_rounds_the_buffer_size` / `viewport_destination_is_the_logical_size_rounded_up`）、
  `render/frame.rs` 1 条（`replacing_a_frame_keeps_surface_and_surface_local_paired`）、
  `render/linux/compositor.rs` 2 条（`a_destination_is_written_only_when_it_changes` /
  `the_destination_is_the_logical_size_rounded_up`）、`render/linux/renderer.rs` 2 条（待应用背景色
  只取一次；shutdown / terminated 后丢弃）、`window.rs` Linux
  `frame_rect_uses_the_platform_buffer_on_linux`（含 0 尺寸 → 1×1）。Linux 专属用例在
  `cfg(target_os = "linux")` 下编译。
- **落点**：`math.rs`（`to_physical_rounded`）、`platform/linux/dpi.rs`（`physical_size` round）、
  `platform/linux/surface.rs`（删 `sync_viewport` / `viewport_applied` / `set_buffer_scale` / IME
  commit；`take_viewport`）、`platform/linux/event_loop.rs`（每轮 `set_surface`、传 background /
  viewport）、`render/linux/renderer.rs`（`new_for_surface(..., background, viewport)` /
  `set_background_color` / `Shared` 背景槽）、`render/linux/compositor.rs`（viewport /
  `last_destination` / present 前 destination / `set_clear_color` / Drop destroy）、
  `render/color.rs`（`background_linear` 覆盖 Linux）、`render/frame.rs`（`surface_local`）、
  `window.rs`（`surface` / `set_surface` / `frame_rect` / `background` 的 Linux `cfg`）。

> 第八轮（V-122 残留：`Committed` ⇒ publish / surface 归属 / min size / setup 顺序）见 §15.12。
> 第九轮（无 viewporter 的整数回退）见 §15.13。

### 15.12 第八轮闭环：V-122 残留（2026-11-15）

第七轮之后剩下的 V-122 ⑧（`Committed` 不 publish）、⑨（renderer 挂载后的 setup 早退
drop-order）、⑦（min size 物理 / 逻辑）与 F-186 / F-189 的 surface 归属残留，本轮一次收掉。
跨平台帧契约只在「有 renderer 的 `Committed` 帧恰好 publish 一次」上收紧，`App::frame` 的签名
与返回类型不变。偏差登记见 [`DEVIATIONS.md`](DEVIATIONS.md) §34（F-193 … F-196 / S-145），
实机证据见 [`linux-verification.md`](linux-verification.md) §4.8 与
[`macos-verification.md`](macos-verification.md) §2.12。

| 项 | 结论 |
|---|---|
| F-193 `Committed` ⇒ publish | `commit_frame` 用纯函数 `frame_reaches_gpu(plan, renderer_surface, surface)` 判定本帧是否产生 GPU 工作：无 renderer（headless / 纯 CPU 帧）⇒ `true`（仍按 `Committed`）；有 renderer ⇒ `plan != Plan::Idle \|\| renderer_surface != Some(surface)`。false 时**先 `tree.settle_frame()`** 再按 `FrameOutcome::Idle` 结算（`scheduler.abort`、不推进 `FrameIndex`、不 publish）——paint 已消费 lane 与 damage，节点状态必须像已提交帧一样复位。`submit_gpu` 删除「`Plan::Idle` + surface 相同 ⇒ 跳过 publish」早退，改为接收算好的 `Plan`（目标尺寸仍取 `physical_surface()`）；有 renderer 的 `Committed` 帧恰好 publish 一次。目的：Linux `FrameGate` 只认 present 后的 `wl_callback`，旧实现中「Committed 但没 publish」的帧会让门禁等 200 ms 活性兜底（V-122 ⑧ / P2-2） |
| F-194 surface 归属 | `Window.surface_from_platform: bool`；`set_surface` 在 clamp 之后、同值 early-return **之前**置 `true`；`set_size` / `set_scale` 只在该标志为 `false` 时把 `surface` 重算成 `size × scale` 的 covering，平台真值写入后保留。headless / 快照 / 平台首次写入前行为不变。Windows 最小化 + DPI 变化时跳过 `set_surface`，现在保持上一次平台写入的真实客户区（恢复时 `WM_SIZE` 覆盖）；macOS `sync_state` 与 Linux 每轮都是 setter → `set_surface`，顺序不变。消除 F-186 / F-189 残留：应用代码在活窗口上调用 `set_size` / `set_scale` 不再把渲染目标重置成 covering |
| F-195 Linux min size | `xdg_toplevel.set_min_size` 收 window geometry 坐标；本仓 `wp_viewport.set_destination(ceil(logical))` 使 surface 尺寸 = 逻辑尺寸（S-145），故直接给 320×200。`apply_min_size(toplevel)` 只在 `WaylandState::new` 的首个 `surface.commit()` 前设置一次；删除 `event_loop` 的 DPI 重发与重复 setup 调用，删除失去唯一调用者的 `WaylandState::toplevel()` |
| F-196 Linux setup 顺序 | `event_loop::run` 把所有可失败 setup（`WaylandState::new` / roundtrip / devices / id / calloop `EventLoop` / ping / `WaylandSource` / `UnixA11y::new`）排在 `Renderer::new_for_surface` 与 `window.set_renderer` 之前；循环路径仍显式 `drop(app)`。早退时 `app` 内没有 renderer，不会出现 `WaylandState`（`wl_surface`）先于 `Renderer` 释放。panic 展开仍是局部变量先于函数参数 `app` 释放（release `panic=abort`），本轮不覆盖 |
| S-145 坐标语义 | `set_min_size` 是 window geometry（surface-local 逻辑）坐标，不是 buffer 像素；**没有 viewporter 时**「surface = 逻辑尺寸」不成立，写入的 320×200 不保证是期望的最小客户区（V-122 ④ / P2-5）。buffer 由客户端按 `round(logical × scale)` 算（F-191），compositor 只缩放到 destination。**2026-11-15 第九轮（§15.13 / §35 F-197）**：整数回退下 `buffer_scale = N` 使 surface-local = `buffer / N = logical`，两条路径的等价都成立；本条只描述 §35 之前的缺口 |

**三平台实测**（HEAD `6a686cd`，工作树 clean）：

| 平台 | 门禁 | 真机 probe |
|---|---|---|
| Windows（主机 125% DPI） | `cargo test -p ui_core --lib` **712 passed / 0 failed / 1 ignored**；`--features "hud,test-support"` lib **774 passed / 1 ignored**（frame_record 6 / hud 7 / layout 4 / snapshot 8 / demo 37）；`clippy -D warnings` exit 0；`fmt --check` exit 0；`ui_core_SKIP_SHADERS=1 cargo check --target aarch64-apple-darwin`（默认 / features）exit 0；stub `pkg-config` + `PKG_CONFIG_ALLOW_CROSS=1` 的 `x86_64-unknown-linux-gnu`（默认 / features）exit 0 | 初始客户区 1200×925；`SetWindowPos` +300 宽 → 1500×925；再 +200 高 → 1500×1125；260 步 ×3 px 右边界拖动 → 2280×925。主卡片填充 bbox 恒 `(32,102,721,216)`（456/456 流式帧）、绘制盒恒 725×220（baseline / 拉宽 / 加高 / sweep / final）；右侧新暴露条带 median **(11,14,19)**（384/384 采样），底部条带 bg_frac 1.0；标题区 Laplacian **949.63** 恒定（heading 1012.967、卡片内部 990.209 恒定）；`WM_CLOSE` 与 `Esc` 均 exit 0；无 `0x887A0001` |
| macOS（2x Retina；单显示器 1680×1050 逻辑 / 3360×2100 backing） | `cargo test -p ui_core --lib` **670 passed / 0 failed / 0 ignored**；`--features "hud,test-support"` 全 target **792 passed**（lib 730、frame_record 6、hud 7、layout 4、snapshot 8、demo 37；Metal golden 8/8 真跑）；clippy / fmt exit 0 | 初始 CGWindow 960×778 → AX 1200×800 → 960×740；三种尺寸主卡片填充 bbox **完全相同** `(25,119)–(602.5,292.5)`，标题区 Laplacian **1744.737** 恒定，card_px **1,634,783** 在 960×778 与 1200×800 相同；960×740 vs 1200×800 共同区域仅 **464 px** 不同（0.0163%），960×778 vs 960×740 **225 px**（0.0079%）；右侧 / 底部新区域是背景 `#0B0E13`、无黑边 / 旧内容；exit 0，无 panic / Metal validation |
| Linux（headless sway 1.11 / wlroots 0.19 / lavapipe，`HEADLESS-1` 1280×720） | `cargo test -p ui_core --lib` **664 passed / 0 failed / 0 ignored**；`--features "hud,test-support"` lib **724** + frame_record 6 / hud 7 / layout 4 / snapshot 8 / demo 37（62）；nightly clippy `-D warnings` 与 fmt exit 0 | min size：`WAYLAND_DEBUG=1` 记录 `-> xdg_toplevel#9.set_min_size(320, 200)` 仅 **1 次**，顺序 set_app_id → set_title → set_min_size → 首个 `wl_surface.commit()`；`preferred_scale(150)`；运行中 1.25→1.0→1.25 两次 scale 切换后 set_min_size 计数仍为 1；1.0 复测同样 1 次 `(320,200)`。resize / 退出：1.25 sweep `1100×800`（输出钳到 1024×576）→ `900×700` → `700×560` → `500×400` → `100×100`，每步 alive、`grim` 截图 1280×720 正常，日志无 panic / VUID / validation；`wtype -k Escape` 两次均 exit 0；60 s soak（20 ms 间隔、2043 次）全程存活、错误 0（最终 rect 500×375）。失败路径：`WAYLAND_DISPLAY=wayland-999` → exit 1 `WaylandCompositor`；`VK_ICD_FILENAMES=/nonexistent.json`（calloop / a11y 之后、renderer 挂载前）→ exit 1 `UnsupportedGpu`，两者无 panic / 段错误 |

- **sway 不执行 min size**：sway 1.11 的 `resize set` 不执行 xdg min_size（1.25 下
  `resize set 100 100` 得到 100×100；CSD / SSD 均如此），因此「compositor 实际钳到 320×200」
  缺实机几何证据，需 Mutter / KWin 或真实指针拖拽（V-122 ⑦）。协议侧证据成立（1 次、
  逻辑值、顺序正确、scale 切换不重发），不能把它当成几何钳制已实测。
- **未验证 / 限制**：
  - Windows：456 帧 / 9.79 s = **46.6 Hz**；sweep 最宽处 **34.7 Hz**（GDI 抓屏 2280×520 的
    成本）；2/384 流式采样在 resize 最右缘出现约 **8 px 黑边**、下一帧恢复（未形成持久黑 /
    白条，不能排除采集时序）。
  - macOS：本机无 Screen Recording 授权，截图是进程内 `CGWindowListCreateImage`（注入
    `/tmp` dylib）抓的**真实窗口 surface**，不是桌面合成截图；单显示器固定 2x，**未在真实
    窗口上制造 DPI / scale 变化**（该路径由单测覆盖）。
  - Linux：`--features` 的 snapshot 测试打印 22 条 `VUID-vkCmdDraw-None-09600`（8/8 仍 pass；
    demo 运行 0 条），旧日志已存在，建议另立 issue，不写成本轮回归。V-122 ①–③⑤⑥ 仍未验；
    ④（P2-5）已由第九轮（§15.13 / §35 F-197）实现并验证。
- **新增 / 相关单测**：`frame_reaches_gpu_tracks_plan_and_target_size`（5 种组合：Full /
  Partial 有工作；`Idle` + 无 renderer 仍 `Committed`；`Idle` + 同尺寸不 publish；`Idle` +
  目标尺寸变化仍 publish）、`set_size_and_set_scale_keep_a_platform_written_surface`（平台真值
  保留；同值 `set_surface` 之后 `set_scale` 不得重算；full damage / 根 `LAYOUT` / `scale_epoch`
  语义不变）、Windows 真实 D3D12 离屏 `an_idle_plan_with_a_real_renderer_settles_node_damage`
  （越界节点 → `Plan::Idle`、不 publish、`FrameIndex` 不变、damage 已复位）。既有
  `idle_frame_does_not_commit_or_increment` 继续覆盖无像素的 Idle 分支；Linux / macOS 的
  `frame_rect_*` 用例同步改为「平台真值保留」。
- **落点**：`window.rs`（`frame_reaches_gpu` / `commit_frame` / `submit_gpu` /
  `surface_from_platform` / `set_surface` / `set_size` / `set_scale` / `frame_rect` 文档）、
  `node/tree.rs::settle_frame` 文档、`render/compositor.rs::Plan::Idle` 文档、
  `platform/linux/window.rs`（`apply_min_size`）、`platform/linux/surface.rs`（建窗调用）、
  `platform/linux/event_loop.rs`（setup 顺序、删 DPI 重发）、`platform/{windows,macos}/event_loop.rs`
  与 `platform/macos/window.rs` 的注释。

### 15.13 第九轮闭环：无 viewporter 的整数回退（V-110 / V-122 ④，2026-11-15）

§15.11 闭环了 fractional 路径，但「compositor 不提供 `wp_viewporter`」时仍没有回退：fractional
对象照建、buffer 按分数放大而 `buffer_scale` 保持 1（1.25 下 surface 被放大 1.25×）。本轮以
**viewporter 为 fractional 的门控**：只有 `wp_viewporter` 与 `wp_fractional_scale_manager_v1`
同时存在才创建 fractional 对象与 `wp_viewport`，否则不创建二者、走整数 `wl_output.scale` +
`wl_surface.set_buffer_scale(N)`；同时把 Wayland 逻辑尺寸取整，保证 buffer 是 `buffer_scale`
的整数倍。偏差登记见 [`DEVIATIONS.md`](DEVIATIONS.md) §35（F-197 / F-198 / S-146 / S-147 /
V-124），实机证据见 [`linux-verification.md`](linux-verification.md) §4.9。

**决策表**（依据：Chromium / Qt / GTK 的成熟实现 + 协议原文 + sway 实测，详见 DEVIATIONS §35
S-146 / S-147）：

| 项目 | 做法 | 代码位置（文件:函数） |
|---|---|---|
| fractional 门控 | `WpViewporter` 存在才 bind fractional manager；两者都成功才 `get_viewport`；`uses_viewport = viewport.is_some()`；工厂对象创建子对象后 `destroy()`（不影响子对象） | `platform/linux/surface.rs::WaylandState::new` |
| 整数 scale 选择 | `preferred_buffer_scale`（surface v6）→ 已 enter 的最大 `wl_output.scale` → 首次 enter 前全部已绑定 output 最大 → 上一个值 → 1（clamp ≥1） | `platform/linux/surface.rs::choose_integer_scale` / `WaylandState::integer_scale_value` |
| `buffer_scale` 管道 | `Window::set_buffer_scale(N)` → `FrameRequest.buffer_scale` 同帧过线 → GPU 线程 present 前 `wl_surface.set_buffer_scale(N)`（与 attach/commit 同一次 commit，`last_buffer_scale` 去重） | `window.rs::Window::set_buffer_scale` / `render/frame.rs::FrameRequest.buffer_scale` / `render/linux/compositor.rs`（present 前写入） |
| 逻辑尺寸整数化 | `WaylandState::new` 把请求 size `.round().max(1.0)`；整数回退 buffer = `logical × N`（否则 `round(L × N)` 可能不是 N 的倍数，`attach` 报 `invalid_size`）；compositor `debug_assert`（`buffer_scale ≥ 1`、buffer 是 N 的整数倍） | `platform/linux/surface.rs::WaylandState::new` / `render/linux/compositor.rs`（`debug_assert` 不变量） |
| 动态 output / leave | 启动时 `WlRegistry::bind` 逐个绑定所有现存 `wl_output`（`GlobalList::bind` 只绑第一个）；`wl_registry.global` 动态绑定、`global_remove` 解绑（v3+ `release`，v1/v2 丢代理）；`enter/leave` 维护 entered 列表；`entered_once` 后 leave 清空列表但保留上一个整数 scale（避免最小化 / 切工作区的虚假 `DpiChanged`） | `platform/linux/surface.rs::bind_existing_outputs` / `unbind_output` / `Dispatch<wl_surface::WlSurface>` |
| 事件循环镜像 | 启动与每轮 `set_buffer_scale`（同值 no-op）、`set_surface(round(logical × scale))`；scale 变化 push `DpiChanged`，随后 `set_scale` / `set_surface` / `set_buffer_scale` 并出帧 | `platform/linux/event_loop.rs::run` / `platform/linux/dpi.rs::physical_size` |

**机制表**：

| 项 | 结论 |
|---|---|
| fractional 路径（有 viewporter） | buffer = `round(logical × preferred_scale/120)`；`wp_viewport.set_destination(ceil(logical))` 由 GPU 线程在 `vkQueuePresentKHR` 前写；`wl_surface` 的 buffer scale 保持默认 1（不调用 `set_buffer_scale`） |
| 整数回退（无 viewporter / 无 fractional manager） | 不创建 fractional / viewport；`N` 按上表选择；buffer = `round(logical × N)`（逻辑尺寸已取整 ⇒ `logical × N`）；`buffer_scale = N` 在 present 前写入，surface-local 尺寸 = `buffer / N = logical`，不会出现「buffer 按分数放大、`buffer_scale = 1`」的 1.25× 放大 |
| 提交顺序 | `set_buffer_scale(N)` → WSI `attach(buffer)` → `commit`；`set_buffer_scale` 与 destination 互斥，且都与 buffer 同一次 `wl_surface.commit` 生效（WSI 规范要求窗口系统变更先于 `vkQueuePresentKHR`） |
| 不变量 | `buffer_scale ≥ 1`；`N == 1` 或 buffer 两维都能被 `N` 整除（否则 `wl_surface.invalid_size`）；逻辑尺寸是整数（`round().max(1.0)`），configure 本来就是 u32 |
| 动态 output | 热插拔的 `wl_output` 动态绑定 / 解绑；多屏 enter 两个不同 scale 的 output 时取已 enter 的最大值（避免糊）；leave 清空 entered 后保留上一个值，不产生 1× ↔ 2× 的抖动 |
| 测试 | `platform::linux::surface::tests` 6 条 + `window::tests::buffer_scale_defaults_to_one_and_clamps`；Linux lib 671（无 features）/ 731（features） |

**三平台 / 实测**（HEAD `240aadc`，headless sway 1.11 / wlroots 0.19 / lavapipe，
`HEADLESS-1` 1280×720）：

| 平台 | 门禁 | 实测 |
|---|---|---|
| Linux | `cargo test -p ui_core --lib` **671 passed / 0 failed / 0 ignored**；`--features "hud,test-support"` lib **731** + frame_record 6 / hud 7 / layout 4 / snapshot 8 / demo 37（62），全 pass；nightly clippy `-D warnings`、fmt `--check` exit 0 | 基线（有 viewporter）：1.25 tree 701×501 @100,37、`set_destination(701,501)` → `attach(876×626)` → `commit`、`set_buffer_scale` grep 0、`create_buffer(876,626)`；2.0 tree 640×360、destination 640×360、buffer 1280×720、无 `set_buffer_scale`。回退（临时隐藏 viewporter，已还原）：1.25 无 `get_viewport` / `set_destination`、`set_buffer_scale(2)` **1 次**且在 attach/commit 之前、`create_buffer(1402,1002)`、tree 701×501；截图窗口物理 bbox 与基线一致 **876×627（125,46–1001,673）**、PIL mean abs diff **2.115/255（0.83%）**、max 217、>8 像素 9.68%（只在字形 / 圆角边缘）、Laplacian 977.06 → **1161.99（+18.9%）**、卡片文字 1984.6 → **2386.7（+20.3%）**；2.0 `set_buffer_scale(2)`、buffer 1280×720、无 destination、tree 640×360、截图 sha256 与基线完全相同（`97aca6fc…`）、Laplacian 592.3257 不变。动态 scale（单进程）：1.0（640×360）→ 2.0（`set_buffer_scale(2)` → attach 1280×720）→ 1.0（`set_buffer_scale(1)` → attach 640×360），三步 rect 恒 640×360@320,180、exit 0；scratchpad leave / 再显示存活、恢复 640×360、Esc exit 0 |
| Windows（主机，HEAD `240aadc`，未设 `ui_core_SKIP_SHADERS`） | lib **712 passed / 0 failed / 1 ignored**；`--features "hud,test-support"` lib **774 / 1 ignored** + frame_record 6 / hud 7 / layout 4 / snapshot 8 / demo 37（62），全 pass；clippy `-D warnings`、fmt `--check` exit 0；`aarch64-apple-darwin` 交叉（默认 + features）exit 0；`x86_64-unknown-linux-gnu` 交叉（stub `pkg-config`，默认 + features）exit 0 | 本轮无 Windows 代码路径改动；`Window::set_buffer_scale` 是 Linux-only 字段，Windows 行为不变 |
| macOS（交叉 `aarch64-apple-darwin`） | `cargo check -p ui_core --target aarch64-apple-darwin`（默认 + features）exit 0 | 本轮无 macOS 行为改动；`FrameRequest.buffer_scale` 与 `Window` 字段都在 `#[cfg(target_os = "linux")]` 下，平台路径不变；无 macOS 运行时验证 |

**未验证 / 限制**（V-124）：

- KWin / Mutter 的真实「无 fractional 客户端」路径未跑（只有源码事实：`wl_output.scale = ceil`）；
  混合 DPI 多屏的 enter / leave、热插拔显示器、真实指针拖拽都未实测。
- 1.25 字形栅格差异：整数回退是 2× 渲染后由 compositor 降采样，fractional 是 1.25× 原生栅格；
  两者无几何 / 布局差异，不视为缺陷。fractional 首帧可能先按整数 scale 出图（`preferred_scale`
  晚到；destination 保证几何正确）。
- 既有 1.25 demo 浮动窗口自振荡（1020×526 ↔ 960×576）在 `HEAD~1`（`a6b7192`，无回退代码）
  同样复现，不是本提交引入；验证时用 SIGSTOP 冻结目标帧截图。

**新增单测与落点**：

- `platform/linux/surface.rs`：`choose_integer_scale` 纯函数 + 6 条用例
  （`preferred_buffer_scale_wins_over_outputs`、`entered_outputs_win_over_bound_outputs`、
  `before_the_first_enter_the_bound_max_is_a_conservative_estimate`、
  `after_the_first_enter_an_empty_list_keeps_the_previous_scale`、
  `no_scale_information_at_all_falls_back_to_one`、`a_non_positive_scale_is_clamped_to_one`）；
  `window.rs`：`buffer_scale_defaults_to_one_and_clamps`。
- 落点：`platform/linux/surface.rs`（viewporter / fractional 门控、`choose_integer_scale`、
  `bind_existing_outputs` / `unbind_output`、`wl_registry` / `wl_output` / `wl_surface.enter/leave`、
  逻辑尺寸取整、`buffer_scale` / `surface_clone` / `take_viewport`）、`platform/linux/event_loop.rs`
  （启动与每轮 `set_buffer_scale`）、`platform/linux/dpi.rs`（模块文档）、`window.rs`
  （`buffer_scale` 字段与 `set_buffer_scale`、`FrameRequest` 构造）、`render/frame.rs`
  （`FrameRequest.buffer_scale` 契约与配对测试）、`render/linux/compositor.rs`（present 前
  `set_buffer_scale` + `debug_assert` 不变量）、`render/linux/renderer.rs`（`wl_surface` 副本）。

