# Windows D3D12 后端审查

审查对象：`crates/ui_core/src/render/windows/`（`compositor.rs`、`renderer.rs`、`device.rs`、`queue.rs`、`swapchain.rs`、`atlas.rs`、`pipeline.rs`、`readback.rs`、`timer.rs`）及相关 `render/shaders/*.hlsl`。HEAD：`a9a4220`。只读审查，未修改源码。

## 总体判断

基础 D3D12 功课总体扎实：每个 in-flight 槽独立 allocator/list、fence 按提交值记账、`SetEventOnCompletion` 唤醒而不是忙轮询、帧延迟 waitable + `SetMaximumFrameLatency(1)`、PSO/根签名/描述符堆一次性创建、上传缓冲和 staging 持久映射、按 `FrameIndex` 延迟回收资源。这些方向与 D3D12 官方最佳实践一致。

但该后端目前不适合直接作为高频交易/复杂图表应用的 GPUI 替代品上线，主要风险集中在四点：

1. **正确性**：`FrameOp::Instances` 把 `start` 传给 `DrawInstanced` 的 `StartInstanceLocation`，而 D3D12 的 `SV_InstanceID` 不随该参数偏移；一旦一帧中出现 `Path/Geometry` 与 quad 混排（图表必然出现），第二段及之后的实例会画错（P0-1）。
2. **多窗口架构**：每个窗口独立 `ID3D12Device`、独立 PSO 集、独立 glyph/image 图集纹理与 staging；多窗口下 VRAM/初始化/每帧图集分发全部线性放大（P1-1、P1-4）。
3. **局部呈现执行方式**：每个 damage 矩形都重放整条 draw list，最多 16 个 damage 矩形就是 16× draw call/顶点处理，抵消了 `Present1` dirty rect 的收益（P1-2）。
4. **健壮性**：所有关键等待都是 `INFINITE`，没有超时/device removed 检查；device lost 或 GPU 挂起时退出/重建可能永久卡死（P1-3）。

当前 `GeometryStore` 只在测试路径注册（`pipeline.rs:546-548` 的 `#[cfg(test)]`），所以 P0-1 在现有产品路径可能暂时不触发；一旦接通 Path/Geometry（图表必需），它就是首帧即错的正确性缺陷。

## 发现（P0-P3）

### P0-1 多段 `FrameOp::Instances` 的实例基址错位：第二个及之后的实例段全部画错

- **证据**：`crates/ui_core/src/render/windows/compositor.rs:1522`：`DrawInstanced(4, len, 0, start)`；`crates/ui_core/src/render/shaders/quad.hlsl:11-13`、`:34`：`GpuPrimitive p = instances[instance_id]`、`o.uv = lerp(p.uv.xy, p.uv.zw, corner)`。实例段的产生见 `crates/ui_core/src/paint/instance.rs:266-275`（`flush_run`）与 `:401-423`（`Path/Geometry` 前先 flush）；单元测试 `paint/instance.rs:1011-1040` 明确构造了 `Instances{start:0,len:1} → Geometry → Instances{start:1,len:1}` 的帧。
- **问题**：D3D12 的 `StartInstanceLocation` 不改变 `SV_InstanceID`（DX12 的 InstanceID 始终从 0 计数，即使 `StartInstanceLocation != 0`；见 DXC issue [#2946](https://github.com/microsoft/DirectXShaderCompiler/issues/2946)）。`quad.hlsl` 用 `SV_InstanceID` 直接索引 `instances`，因此每个 `Instances` op 都读取 `instances[0..len]`，`start` 被忽略。只有 `start == 0` 的第一段正确。
- **影响**：一旦场景中 `Path/Geometry` 后面还有 quad/glyph/image（图表中网格线/曲线后接蜡烛或文本是常态），后续整段实例会重复绘制第一段实例，表现为缺少图元、重复图元、错误颜色/位置；这是静默的错误像素，不会报 D3D 错误。由于当前 `GeometryStore` 仅测试可注册，属于“接通 Path 后立即爆”的 P0。
- **成熟实现对照**：Metal 后端使用 `baseInstance = start`（`render/macos/compositor.rs:700-706`），Vulkan 后端使用 `firstInstance = start`（`render/linux/compositor.rs:1433`），两者都正确地按实例基址偏移；D3D12 需要显式把 base instance 传给 shader（root constant 或 SRV 偏移），不能依赖 `StartInstanceLocation`。
- **建议**：在 `draw_ops` 为每个 `FrameOp::Instances` 设置一个 `instance_base` root constant，shader 索引改为 `instances[instance_id + instance_base]`；或为每个实例段绑定不同的 SRV 起始偏移。增加一个“Path/Geometry 前后各一批 quad”的像素级回归测试（当前只有编码层断言，没有渲染断言）。
- **置信度**：高。

### P1-1 每个窗口独立 Device/PSO/图集纹理，多窗口成本线性放大

- **证据**：`crates/ui_core/src/render/windows/device.rs:171-177`（`Device::new()` 每次调用都创建新的 DXGI factory 和 `ID3D12Device`）；`crates/ui_core/src/render/windows/compositor.rs:240`、`:274-301`（每个 `GpuRenderer` 都创建自己的 `Queue`、`Pipeline`、`AtlasTextures`、RTV 堆）；`crates/ui_core/src/render/windows/renderer.rs:139-145`（每个窗口 `GpuRenderer::new_for_composition`）；`crates/ui_core/src/app.rs:513-566`（图集页逐窗口 `queue_atlas_upload`，注释 `window.rs:849-853` 明确“纹理是每窗口一份”）。
- **问题**：N 个窗口 = N 个 D3D12 device + N 条 direct queue + N 套 4 个 PSO/根签名 + N 份 glyph/image 图集纹理与 staging。图集页最大 2048²：glyph R8 约 4 MiB/窗口，image BGRA8 约 16 MiB/窗口（`render/atlas.rs:21`、`:36`），加上每窗口约 4.5 MiB 的常驻 staging；4~8 个窗口仅图集/上传堆就可达数十至上百 MiB。每次图集页或脏行更新，UI 线程还要为每个窗口 `to_vec()` 并逐窗口上传同一份像素。
- **影响**：多窗口是产品核心需求；当前设计使打开新窗口的 VRAM、PSO 编译时间和图集分发 CPU 成本按窗口数线性增长，和 Chrome/Qt 的单 GPU 进程/多窗口共享模型相反；高频行情下每个窗口独立 device 也放大了驱动调度和 fence 开销。
- **成熟实现对照**：Chrome 的 GPU 进程通常用一个 `ID3D12Device`/GPU 线程服务多个窗口，glyph atlas 和 PSO 在进程级共享；Qt RHI 也支持 device/资源在多个 swapchain 间复用。多窗口 GUI 不应为每个窗口复制整张 2048² 图集。
- **建议**：至少将 `Device`、`Queue`、`Pipeline`、`AtlasTextures` 提升到进程级（单 device 多 swapchain/多 DComp visual）；如未来要做跨进程/多 device，图集用 shared handle 或每个 device 只保留必要页。图集上传按“页版本 + 脏行”只推送给一个共享纹理，而不是逐窗口广播。短期若必须保留每窗口 device，应把图集改为按窗口懒分配并按需共享，避免每个窗口都常驻全页。
- **置信度**：高。

### P1-2 每个 damage 矩形重放整条 draw list：最多 16× draw call/顶点工作量

- **证据**：`crates/ui_core/src/render/windows/compositor.rs:1243-1262`：`for rect in damage.as_slice() { ... draw_ops(...) }`；`draw_ops` 在 `:1500-1561` 遍历 `payload.ops()`，对每个 `Instances` 段无条件 `DrawInstanced(4, len, 0, start)`，对每个 `Geometry` 无条件 `DrawIndexedInstanced`，完全不检查该 op/实例是否与当前 damage 矩形相交。`render/damage.rs:17` 的 `MAX_DAMAGE_RECTS = 16`；测试 `compositor.rs:2182-2189` 明确断言“两个 damage scissor 就把 draw op list 重放两遍”，`draw_calls == 2`。
- **问题**：局部帧的绘制复杂度是 `damage_rect 数 × 全场景 op/实例数`，而不是只处理脏区域内的图元。一个交易图表常见的多脏区（十字光标、tooltip、盘口刷新、状态栏动画）可能产生 4~16 个矩形；即使 scissor 会剔除 fragment，顶点着色、实例读取、draw 提交仍然按矩形重复执行。
- **影响**：在“复杂图表 + 多个独立刷新区域”的目标场景下，局部帧可能比全量帧还慢；dirty rect 只优化了 copy/present，没有优化 draw，反而让 CPU/GPU 在 damage 矩形多时放大 16 倍。这是与高频图表需求直接冲突的性能缺陷。
- **成熟实现对照**：Chrome 的 viz/cc 以 tile 为最小重绘单位，只为 dirty tile 生成/执行 draw，不会把同一份 display list 对每个 dirty rect 重放；Qt Quick 也把 damage 汇总为一次 render pass，用 scissor/裁剪剔除未受影响 item。ANGLE/D3D 后端通常在 CPU 侧先把 damage 与 draw 包围盒求交，再只提交相交 draw。
- **建议**：在编码阶段给每个 `Instances` 段记录实例 AABB（或按 primitives 分组），录制时对每个 damage 矩形剔除不相交的 op/实例；更彻底的做法是按 damage 矩形生成“可见实例索引列表”或按 tile 分桶，再各绘制一次。至少应做到一个 damage 矩形内只提交与其相交的 draw，而不是无条件遍历全场景。
- **置信度**：高。

### P1-3 设备丢失/GPU 挂起时全链路 `INFINITE` 等待，退出/重建可能永久卡死

- **证据**：`crates/ui_core/src/render/windows/queue.rs:289-306`：`wait_for` 使用 `WaitForSingleObject(self.event, INFINITE)`；`crates/ui_core/src/render/windows/renderer.rs:1066-1068`：`wait_for_pair` 使用 `WaitForMultipleObjects(..., INFINITE)`；`renderer.rs:1087-1117`：GPU 线程 join 用 `MsgWaitForMultipleObjects(..., INFINITE, ...)`；`crates/ui_core/src/render/windows/compositor.rs:1067-1073`：`GpuRenderer::Drop` 先 `queue.wait_idle()`；`queue.rs:179-181` 的 `wait_idle` 最终也走无限等待。
- **问题**：所有 fence/事件等待都没有超时，也没有在等待期间检查 `GetDeviceRemovedReason`。D3D12 device 被移除或 GPU 挂起时，pending fence 是否还能 signal 取决于驱动/移除原因；一旦 fence 不再 signal，GPU 线程会卡在 `wait_idle`/`wait_slot_fence`，UI 线程的 `join_with_message_pump` 又无限等待线程句柄，最终窗口无法关闭、设备无法重建。`Device::note_failure` 只在“已经返回错误”的调用点被触发；如果线程正阻塞在 fence 上，根本没有机会调用它。
- **影响**：设备丢失（TDR、驱动更新、显卡切换、远程桌面）在交易终端上是现实事件。当前不是“检测到 device lost 后优雅退出”，而是可能永久卡死；这对多窗口长时间运行的应用是 P1 级可靠性问题。
- **成熟实现对照**：Chrome 的 GPU watchdog 对 GPU 线程/进程有超时和 `GetDeviceRemovedReason` 检查，超时后杀死/重建 GPU 进程；D3D12 常规做法是 fence 等待带 timeout，超时后查询 device removed reason 并走 device-lost 恢复/退出路径，而不是 `INFINITE`。
- **建议**：为所有 fence/交换链等待增加有限超时（例如 1~2 秒，结合帧预算），超时后调用 `GetDeviceRemovedReason`；把 device lost 传播为 GPU 线程错误并让 `Drop`/join 有上限。长期应实现 device lost 后的资源重建（至少支持进程级重建设备与所有窗口 swapchain）。`GpuRenderer::Drop` 不应在 device 已丢失时继续无限 `wait_idle`。
- **置信度**：中高（“无超时”与“无设备健康检查”是确定的；是否每次 device lost 都卡死取决于驱动 fence 行为，需用 `ID3D12Device5::RemoveDevice` + `wait_idle` 实测）。

### P1-4 窗口创建在 UI 线程同步建 Device/PSO/交换链并 prime，阻塞 UI

- **证据**：`crates/ui_core/src/platform/windows/event_loop.rs:611-631`：每个窗口依次在 UI 线程执行 `NativeWindow::create` → `Renderer::new_for_hwnd` → `set_geometry` → `show`；`renderer.rs:131-145` → `compositor.rs:233-272` 的 `new_for_composition` 创建 `Device::new()`、`Pipeline::new()`、两条组合交换链，并对两条交换链各 `prime_detached` 一次（内部 `Present1` + `wait_slot_fence` 同步等 GPU）；`pipeline.rs:95-185` 每次 `Pipeline::new` 同步创建根签名 + 4 个 PSO（含 DXIL PSO 编译）。
- **问题**：窗口创建路径把“设备/管线初始化 + 交换链创建 + 首帧 prime 完成”全部放在 UI 线程同步执行，GPU 工作与 PSO 编译没有被移交给 `kytrix-d3d12` 线程；打开/新增窗口时 UI 消息泵被阻塞，且 `prime_detached` 的 `wait_slot_fence` 是无限等待（叠加 P1-3 的 device lost 风险）。
- **影响**：每开一个窗口，UI 线程要承担设备创建/适配器枚举、4 个 PSO 编译、两次 swapchain prime 和 GPU fence 等待；D3D12 device/PSO 创建通常是毫秒到数十毫秒级，窗口越多启动越慢，且打开新图表窗口时 UI 会明显卡顿。对“多窗口 + 低延迟”产品这是直接的用户体验问题。
- **成熟实现对照**：Chrome 把 device/PSO/资源创建放在 GPU 进程的 GPU 线程，UI 线程只创建窗口和提交 swapchain 请求；Qt RHI 也把 RHI 初始化放在渲染线程/专用线程，GUI 线程不阻塞在 PSO 编译和 fence 上。
- **建议**：把 Device/PSO 缓存提升到进程级并在 GPU 线程初始化；窗口创建时 UI 线程只创建 HWND/DComp target，GPU 线程异步创建/挂接 swapchain 并 prime；`ShowWindow` 前允许短暂显示背景色而不是同步等 fence。至少把 `Pipeline::new` 与 `prime_detached` 从 UI 线程移走。
- **置信度**：高（代码路径明确）。

### P2-1 裁剪顶点钳制导致 glyph/image 的 UV 被压缩

- **证据**：`crates/ui_core/src/render/shaders/quad.hlsl:22-26`：当 `clip_index != 0` 时把 `lo/hi` 夹到 `clip_rect`（± `KYTRIX_CLIP_VERTEX_SLACK`）；`:34`：`o.uv = lerp(p.uv.xy, p.uv.zw, corner)` 仍按 0..1 插值。`crates/ui_core/src/paint/instance.rs:595-637`：image/glyph 写入完整 tile 的 `uv` 和当前 clip 的 `clip_rect`。
- **问题**：`quad.vs` 为了把大 quad 收进裁剪矩形而移动顶点，但 UV 仍映射到移动后的 0..1 范围；当 glyph 或 image 只有一部分落在 clip 内（滚动列表边缘、被裁剪的卡片/文字），可见部分会把整张纹理压缩到裁剪后的几何上，产生拉伸/压缩而非正确裁剪。纯色 quad/shadow 不读纹理，因此测试容易漏掉；`geometry.hlsl` 的纹理路径没有这个顶点夹紧，问题集中在 uber-quad 的 glyph/image。
- **影响**：文本滚动、列表裁剪、图片卡片边缘会出现可见纹理变形，属于用户可直接看到的渲染错误；在行情表格/滚动新闻/图表标签中常见。
- **成熟实现对照**：Qt/ANGLE 的纹理 quad 裁剪依赖 scissor/render target clip 或像素 shader 的 clip mask，不会通过移动顶点并保留 0..1 UV 来实现裁剪；正确做法是用原始 world 位置计算 UV，或在 PS 中按 clip 距离 discard。
- **建议**：对 `KIND_GLYPH/KIND_IMAGE` 不要移动顶点；保留原 quad 顶点交给 scissor 裁剪，或在 VS 中按原始 `world_px` 重新计算 `uv = (world_px - origin) / size * (uv1 - uv0) + uv0`，并在 PS 中应用 clip mask。增加“部分超出 clip 的 glyph/image”像素回归测试（当前 `damage_isolates_pixels` 只测纯色 quad）。
- **置信度**：高。

### P2-2 全量帧仍执行 persist→back buffer 整面 copy + 4 次 barrier

- **证据**：`crates/ui_core/src/render/windows/compositor.rs:1590-1639` 的 `copy_to_back` 每帧对 `persist.texture` 和 `back` 各做 `RENDER_TARGET→COPY_SOURCE` / `PRESENT→COPY_DEST` 再反向转回（`:1599-1637`），`Plan::Full` 在 `:1614-1617` 走 `CopyTextureRegion` 复制整个 content；调用点 `:1266-1275` 对每个提交帧执行。`composition.rs:345-367` 的 buffer 是 content+64 再 256 对齐，persist 与 3 个 swapchain buffer 都按该尺寸常驻。
- **问题**：全量帧（滚动、动画、图表全刷新）在 GPU 上多一次 persist→back buffer 的整面 copy；4K 内容约 33 MB/帧，60 fps 约 2 GB/s 读 + 2 GB/s 写，还不算 persist 自身的 33 MB 常驻和 DWM 背景层。局部帧虽然只 copy damage，但 draw 仍是全量（P1-2）。
- **影响**：中高分辨率/高刷新率下增加显存带宽、GPU 时间和功耗；对集显/笔记本影响更明显。dirty rect 的收益在“全量帧 + 全量 copy”下被抵消，且 3 个 swapchain buffer + persist 使每窗口常驻内存约为内容面积的 4 倍。
- **成熟实现对照**：Chrome/Qt 的常规窗口路径直接把内容渲染到 swapchain back buffer（或用 DComp surface），DWM 负责合成；只有在确实需要保留上一帧内容时才维护额外 offscreen。`Present1` dirty rect 本身不需要一个常驻的 persist 纹理来支撑（`Present1` 文档要求应用只修改 dirty 区域，见 [Present1 文档](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgiswapchain1-present1)）。
- **建议**：实测在目标 GPU/4K 下的 copy 成本；考虑常规帧直接渲染到 back buffer（按 damage scissor），只在 grow/shrink/窗口 resize 的过渡期使用 persist；或把 persist 改为可丢弃的缓存。评估 2 buffer（`SetMaximumFrameLatency(1)` 下 3 buffer 是否必要）与 content 之外的 buffer 裁剪。
- **置信度**：高（代码事实）；性能影响需实测。

### P2-3 未处理 `DXGI_STATUS_OCCLUDED`，被遮挡窗口持续按 vsync 渲染

- **证据**：`crates/ui_core/src/render/windows/swapchain.rs:155-167`：`let hr = Present1(...); if hr.is_ok() { ... }`；`DXGI_STATUS_OCCLUDED` 是成功类 HRESULT（正数），会被当成正常 present，继续 `frame.presented()`。GPU 循环 `renderer.rs:886-961` 每帧等待 waitable、录制、Present；没有向 UI 上报“已遮挡”或暂停帧循环的路径。
- **问题**：窗口被其他窗口完全覆盖/桌面遮挡时，DWM 通过 `DXGI_STATUS_OCCLUDED` 告诉应用“输出完全被遮挡”；当前实现完全忽略，仍按 vsync 消耗 GPU 和 CPU 渲染不可见内容。多窗口交易终端经常有多个图表窗口被遮挡或最小化/半遮挡，持续渲染会浪费功耗并挤占前台窗口 GPU 时间。
- **影响**：多窗口场景 GPU/CPU 占用不随可见性下降；笔记本续航、风扇、前台图表帧率受负面影响。
- **成熟实现对照**：Chrome 的 `SwapChainPresenter` 明确处理 `DXGI_STATUS_OCCLUDED`，把 swapchain 标记为 occluded 并暂停/节流渲染，恢复可见后再继续；Qt 也通过窗口暴露/遮挡状态停止渲染循环。`DXGI_STATUS` 文档说明 Present 可通过该状态告知输出窗口完全遮挡（[DXGI_STATUS](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/dxgi-status)）。
- **建议**：把 `DXGI_STATUS_OCCLUDED` 与 `S_OK` 区分，进入 occluded 状态并暂停 GPU 循环（或降到低频 `DXGI_PRESENT_TEST`）；在 `WM_SHOWWINDOW` / `WM_SIZE` / 遮挡恢复事件或定时恢复时重新唤醒。窗口最小化路径也应在 UI 线程停止发布帧。
- **置信度**：高（代码事实）；具体 DWM 是否在 composition swapchain 上返回 occluded 需在目标 Windows 版本实测。

### P2-4 每帧两次完整实例拷贝 + 每帧描述符/小分配开销

- **证据**：`crates/ui_core/src/render/frame.rs:70-88`：`FramePayload::from_buffer` 在 UI 线程 `extend_from_slice(buffer.as_bytes())` 复制全部实例；`crates/ui_core/src/render/windows/compositor.rs:1420-1434`：`UploadArena::write` 在 GPU 线程用 `copy_nonoverlapping` 再复制一次；`pipeline.rs:221-247`：每帧 `write_slot` 调用两次 `CreateShaderResourceView`；`compositor.rs:833`：每帧 `FrameTimings::new()`，`:858`：`timings.samples().to_vec()` 再分配；`swapchain.rs:111-125`：每帧 `cast::<IDXGISwapChain3>()` + `GetBuffer(index)`。
- **问题**：一帧的实例字节从 `InstanceBuffer`→`FramePayload`→mapped upload buffer 共两次完整 CPU 拷贝；以 50k 个 128 B 实例计算约 6.4 MB/帧 ×2，60 fps 约 768 MB/s，且第一次发生在 UI 线程。每帧还有 2 次 SRV 描述符写、`FrameTimings` 小 Vec 分配、`to_vec` 分配和一次 `IDXGISwapChain3` QueryInterface/GetBuffer，属于可消除的固定开销。
- **影响**：图表越复杂、实例越多，UI 线程越容易被 memcpy 拖慢；与“低延迟”目标冲突。每帧 descriptor 写/分配在多窗口下累计也会增加 CPU 占用。
- **成熟实现对照**：Chrome 使用共享内存命令缓冲/持久映射上传环，实例数据通常只写一次；D3D12 常见做法是每 in-flight 槽一个持久映射 UploadArena，由 UI 生产者直接写入当前槽（用 fence/槽页握手），GPU 线程只提交。SRV 可缓存 `ID3D12Resource*` 到描述符槽的映射，只在扩容时重写；`IDXGISwapChain3` 应缓存。
- **建议**：把 UI 编码目标改成每槽持久映射的实例环（和 `UploadArena` 共享，用 triple-buffer + fence 控制复用），去掉 `FramePayload` 的实例 `Vec` 拷贝；缓存 `IDXGISwapChain3`；`write_slot` 缓存上次写入的 `(resource, count)`，控件不变不重写；复用 `FrameTimings` 的 `Vec`（例如挂在 `SubmittedFrame` 池）。
- **置信度**：高（代码事实）；收益取决于场景实例数。

### P2-5 图集页重建/扩容时 `wait_idle` + 重置全部 in-flight 列表，阻塞所有帧

- **证据**：`crates/ui_core/src/render/windows/atlas.rs:209-242`：`prepare` 检测页尺寸变化后 `queue.wait_idle()`、`queue.reset_in_flight_lists()`，再重建纹理并要求整页上传；`atlas.rs:307-323` 的 `recreate` 同时重建每槽 staging（每条 `StagingSlot::new_with_rows` 都 `Map` 分配）；页尺寸档位见 `render/atlas.rs:21-30`（512→1024→2048）。
- **问题**：字形页从 512 增长到 1024/2048、或首次出现图片页时，GPU 线程会等整个队列 idle 并重置所有 slot 的 allocator/list；这意味着所有在飞帧被丢弃/等待，帧循环出现明显停顿。每窗口独立执行一次，多窗口时同一图集变化会在每个 GPU 线程上各触发一次。
- **影响**：字体/DPI/主题变化或首次插入大图片时出现可感知的掉帧/卡顿；图集页增长在交易应用中可能发生在切换布局/主题时，用户可见。
- **成熟实现对照**：Chrome/Skia 的 glyph atlas 通过纹理 atlas 分页/新增页或 GPU 侧复制扩页，不让整个 device idle；Qt 的 glyph cache 也按页增长，不阻塞其它窗口渲染。至少应使用新的纹理 + SRV 切换 + retire 旧纹理，而不是 `wait_idle` 全部槽。
- **建议**：启动时预分配最大页（2048²）或按当前 DPI 预估一次到位；运行时扩页采用“新纹理 + 新 SRV + 立即整页上传（仅等待上传所需槽）+ retire 旧纹理”的方式，避免重置全部 in-flight 列表。对同一进程的多窗口共享同一页重建事件。
- **置信度**：高（代码事实）；实际卡顿时长需实测。

### P3-1 适配器选择只取第一个非软件适配器，混合显卡可能选到 iGPU

- **证据**：`crates/ui_core/src/render/windows/device.rs:352-375`：`pick_hardware_adapter` 从 `EnumAdapters1(0..)` 返回第一个 `DXGI_ADAPTER_FLAG_SOFTWARE` 未置位的适配器，没有 `IDXGIFactory6::EnumAdapterByGpuPreference` / 高性能偏好 / 用户配置。
- **问题**：在混合显卡笔记本/台式机上，枚举顺序不保证独显在前；可能创建在 iGPU 上，导致复杂图表 GPU 性能/显存不符合预期，且用户无法从应用配置切换。
- **影响**：高频图表/多窗口场景帧率明显低于硬件能力；排查困难。
- **成熟实现对照**：Chrome 通过 `IDXGIFactory6::EnumAdapterByGpuPreference(DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)` 或用户选择/系统设置选择适配器，并在多 GPU 环境提供 fallback/诊断。
- **建议**：优先 `EnumAdapterByGpuPreference(HIGH_PERFORMANCE)`，再回退枚举；记录最终适配器/FL/caps（当前 `device.rs:211-212` 丢弃了 `GpuCaps`），并提供配置覆盖；对 hybrid 切换（eGPU/外接屏）考虑重建。
- **置信度**：高（代码事实）。

### P3-2 背景色变更整条 1×1 swapchain 重建，每次都新建 DXGI factory/swapchain 并同步 prime

- **证据**：`crates/ui_core/src/render/windows/compositor.rs:660-689`：`refresh_background` 每次 `background_dirty` 都 `Swapchain::for_composition(&self.queue, (1,1))` → `prime_detached`（`Present1` + `wait_slot_fence`）→ `SetContent` + `Commit`；`swapchain.rs:65-96` 的 `for_composition` 每次都 `create_factory()`（`:244-248` 新建 `IDXGIFactory4`）和创建 swapchain。
- **问题**：主题切换/背景色动画/多窗口同时换色时，每个窗口、每次颜色变化都创建新的 DXGI factory + 1×1 swapchain + command list + GPU fence 等待。虽然 1×1 像素成本极低，但 COM/内核对象创建、`Present1`/fence 同步在 GPU 线程上串行，频繁改色会造成不必要的延迟和对象 churn；`background_dirty` 只保留最新颜色，连续动画颜色会不断重建旧链。
- **影响**：主题切换/动态背景时窗口背景更新延迟；多窗口下每个窗口独立重建；资源 churn 增加驱动压力。
- **成熟实现对照**：Chrome/Qt 通常用 DComp surface/visual 属性或根窗口背景色更新，不会为每次颜色变化重建 swapchain；若必须保留 1×1 交换链，可缓存一条链并重新 clear/present，或使用可更新的 DComp surface。
- **建议**：保留一条 1×1 背景交换链，颜色变化时只 clear/copy + `Present1`（必要时同步一次），不要重建 factory/swapchain；或者改用 `IDCompositionSurface`/DComp 支持的纯色层。把 `create_factory` 提升为进程级共享。
- **置信度**：高（代码事实）；影响取决于改色频率。

## 做得好的地方

- **in-flight 槽管理正确**：`MAX_FRAMES_IN_FLIGHT=3`，每槽独立 allocator/list，复用前 `wait_slot_fence`，`Queue::submit` 后 `Signal` 推进单调 fence；没有每帧创建 allocator/list（`queue.rs:64-105`、`:120-154`）。
- **不忙轮询**：完成通知走 `ID3D12Fence::SetEventOnCompletion` + `WaitForMultipleObjects`，帧节奏走 DXGI frame-latency waitable；`SetMaximumFrameLatency(1)` 控制 CPU 排队深度（`queue.rs:156-176`、`renderer.rs:1044-1080`、`swapchain.rs:84-90`）。
- **管线对象一次性创建**：根签名、4 个 PSO、RTV/SRV 堆只在构造期创建；根常量 28 DWORD、静态 sampler、每槽 4 SRV 的布局清晰，没有每帧 PSO/根签名重建（`pipeline.rs:95-185`、`:221-265`、`:329-404`）。
- **上传路径成熟**：UploadArena/Atlas staging 持久 `Map`，按 in-flight 槽分桶，容量按需增长并 retire 旧缓冲；`FramePayload` 有回收池，避免每帧大分配（`compositor.rs:1332-1458`、`atlas.rs:49-83`、`frame.rs:44-144`）。
- **资源生命周期**：`RetireQueue` 按 `FrameIndex` 延迟释放，支持跳过帧序号；swapchain back buffer / persist 的 COM 引用显式 `ManuallyDrop::drop` 处理（`retire.rs:40-104`、`compositor.rs:1681-1755`）。
- **设备丢失诊断**：DRED 在设备创建前强制开启，`DeviceLost` 粘性错误并写 DRED 日志；WARP/调试层只用于 test（`device.rs:267-343`、`:377-390`）。
- **DirectComposition 首次呈现顺序**：内容层/背景层都先 prime 再 `SetContent` + `Commit`，避免 `WS_EX_NOREDIRECTIONBITMAP` 窗口在首次 present 前透明；`grow/shrink` 的视觉切换也遵守这个顺序（`compositor.rs:233-272`、`platform/windows/composition.rs:112-176`）。
- **读回/快照隔离**：`readback` 模块仅在 `test`/`test-support` 编译（`render/windows.rs:16-17`），发布物不带 GPU→CPU 读回，不会影响生产性能；离屏快照语义独立。

## 存疑 / 需实测

1. **DComp 视觉坐标单位与 DPI**：代码把物理像素 `client` / `content` 直接传给 `IDCompositionScaleTransform::SetScaleX2/SetScaleY2` 与 `IDCompositionRectangleClip::SetRight2/Bottom2`（`composition.rs:320-327`、`:237-269`）。Microsoft 文档把 `IDCompositionVisual::SetOffsetX` 的 offset 描述为 pixels，但 DComp 历史上也有 DIPs 语义的讨论；需在 125%/150% DPI、多显示器混合 DPI、跨屏拖动下实测内容是否严格 1:1、背景层是否过扫/欠扫。
2. **`Present1(sync=1)` + waitable 双重节流**：GPU 线程先等 frame-latency waitable，再 `Present1(1, ...)` 阻塞到 vblank；可能比 `Present1(0)` 多一帧延迟。需对比输入→光子延迟与 `Present` CPU 时间，确认 Chrome/Qt 在 DComp 路径使用 sync=0 还是 1。
3. **组合交换链 `ResizeBuffers` 限制**：`swapchain.rs:9-12`、`compositor.rs:441-454` 声称首次 Present 后 `ResizeBuffers` 必然 `0x887A0001`；官方文档未明确该限制，需在目标 Windows/driver 上复测。若该限制不成立，可去掉 grow/shrink 整条重建 swapchain 的复杂度和卡顿。
4. **`CopyResource` 在 flip back buffer 上 `DEVICE_REMOVED`**：`compositor.rs:1612-1613` 注释称实测 `CopyResource` 失败才改用逐矩形 `CopyTextureRegion`；需用 D3D12 调试层确认是否真正是资源状态/用法错误（如缺 barrier、COPY_DEST 状态不对），而不是 API 限制。若可 `CopyResource`，全量帧可减少每矩形 `CopyTextureRegion` 开销。
5. **`Present1` dirty rect 在 DComp 上的真实收益**：需用 GPUView/WPFTrace 对比全量 vs dirty 的 DWM composition 时间、显存带宽；1×1 背景层 + 内容层双 swapchain 是否让 DWM 仍处理两层，局部 present 的收益可能小于预期。
6. **device lost 时 fence 行为**：用测试里已有的 `ID3D12Device5::RemoveDevice`（`device.rs:621-648`）扩展“移除后调用 `wait_idle` / `wait_slot_fence`”用例，验证是否永久阻塞；这决定 P1-3 是否升级为“必现卡死”而非“驱动相关”。
7. **`LatestFrame` 跨 surface 的 damage 合并**：`frame.rs:211-219` 合并旧 `Plan` 与新 `Plan` 时没有按最新 `surface` 重新夹紧；窗口先大后小时 pending 的旧 partial damage 可能包含新 surface 之外的矩形，`window.rs:1157-1167` 只有 debug_assert。建议补一个窗口缩小期间连续 publish 的回归测试。
8. **`Readback` 缓存键只有尺寸没有格式**（test-support）：`readback.rs:74-76` 只比较 `self.size`，`recreate` 以 `format` 计算 row pitch（`:172-251`）；同一个 `Readback` 以相同尺寸读 R8 与 BGRA8 会复用错误 row pitch（测试目前用了两个实例规避）。不影响生产，但会影响后续测试可信度。
9. **多窗口 VRAM/帧率基数**：建议在 4 个 4K 窗口、复杂图表场景实测每窗口显存（persist + 3 swapchain + atlas + staging）与 GPU 利用率，以确认 P1-1/P2-2 的实际量级。
