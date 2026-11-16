# Linux Vulkan 后端审查

## 总体判断

审查方式：在 Windows 上对 HEAD `a9a4220` 做静态审查，未在 Linux/真实 GPU 上运行；本文结论除明确标注的代码事实外均为推断。审查范围限定为 `crates/ui_core/src/render/linux/` 及其直接依赖的共享 render 模块，只读，未改源码。

总体判断：这套 Linux Vulkan 后端在**同步原语所有权、帧槽轮转、延迟销毁和 atlas staging**上明显高于常见的“每帧新建一切”的实现，异步提交/retire 的骨架是成熟的；但它目前仍不是可以放心上生产的完成品。主要风险集中在三处：**物理设备/队列候选选择过于随意（可能选到 lavapipe 或 iGPU）**、**swapchain 的 surface lost / capabilities 恢复不完整（失败会直接杀 GPU 线程）**，以及 **damage 路径只省了栅格化，没有省整面 copy 和 draw 重放**。对高频行情/复杂图表这类 4K、多窗口、长时间运行的桌面应用，这三类问题会分别表现为“性能灾难 / 会话恢复失败 / 带宽与 CPU 峰值”。

## 发现（P0-P3）

### P1-1 设备选择取第一个有图形队列的设备，未按 D8 能力筛选，也未排除 CPU/软件适配器

- **证据**：`crates/ui_core/src/render/linux/device.rs:159-195`。循环对每个物理设备只找 `VK_QUEUE_GRAPHICS_BIT` 队列族并检查该族是否支持当前 surface，命中后立即 `break`；`GpuCaps::check()`（`device.rs:223-239`）是在选定设备之后才执行，失败时不会回退到下一块设备。`GpuCaps.device_type` 只被保存/打印（`device.rs:35`、`device.rs:73`），从未参与选择。
- **问题**：`VkPhysicalDeviceType::CPU`（lavapipe/软件 Vulkan）和性能较低的集成 GPU 与独立 GPU 被视为同等候选；前者在开发机若排在前（或 `VK_ICD_FILENAMES` 把 lavapipe 排在前面）会被静默选中。若第一块设备缺 Vulkan 1.2/timelineSemaphore/descriptorIndexing/`VK_KHR_swapchain`，`caps.check()` 直接返回 `UnsupportedGpu`，即使第二块设备完全满足 D8 也不会尝试。
- **影响**：高频行情应用的帧时间可能直接由 CPU 软件光栅决定，表现为“能启动但不可用”；混合显卡笔记本可能永远选 iGPU；多 GPU 机器上还可能因枚举顺序不同出现“同一程序有时能跑、有时报 UnsupportedGpu”。这与 D8“不降级”的硬基线意图不一致。
- **成熟实现对照**：Windows 后端 `crates/ui_core/src/render/windows/device.rs:351-374` 明确跳过 `DXGI_ADAPTER_FLAG_SOFTWARE` 并要求至少一块硬件适配器；Chrome 的 GPU 选择按高性能/低功耗偏好并排除软件适配器；Qt RHI 的 Vulkan 后端优先独显，软件设备只在显式选择时使用。
- **建议**：枚举全部候选后打分：直接跳过 `VK_PHYSICAL_DEVICE_TYPE_CPU`（除非测试环境显式允许），优先 `DISCRETE_GPU`，其次 `INTEGRATED_GPU`；在 `break` 前先完成 `apiVersion`、`VkPhysicalDeviceVulkan12Features`、`VK_KHR_swapchain` 和 surface present 校验，不满足则继续下一块；把最终 adapter 名称/类型写入诊断日志。
- **置信度**：高（静态代码可证；性能影响需在真实 GPU 上量测）。

### P2-1 只检查第一个 GRAPHICS 队列族是否支持 present，未搜索 graphics+present 族

- **证据**：`crates/ui_core/src/render/linux/device.rs:161-190`。先用 `position(|family| family.queue_flags.contains(GRAPHICS))` 取第一个图形族，再只对这个族调用 `get_physical_device_surface_support`；不成立就跳过整块设备。
- **问题**：有些实现会把 graphics-only 队列族排在前面（例如计算/显示分离或多 GPU 配置），真正的 graphics+present 族在后面的索引；此时设备被错误判定为“不支持 Wayland surface”，或需要单独的 present 队列族（Vulkan 允许 present 与 graphics 不同族，需 ownership transfer / concurrent sharing）。
- **影响**：在合法的多队列/多 GPU 配置上错误报 `UnsupportedGpu`，属于“代码看似正确但换机器就启动不了”的兼容性问题。
- **成熟实现对照**：Vulkan 官方 samples / Chrome 都是遍历所有 `VkQueueFamilyProperties`，选择同时支持 GRAPHICS 与 surface 的族；没有这样的族时再考虑独立 present 族并显式处理 queue family ownership。
- **建议**：遍历全部族；优先选择 `GRAPHICS | COMPUTE | TRANSFER` 且 `surface_support == true` 的族；若必须拆分，增加 present 队列族字段和 image ownership 处理；记录最终 queue family 索引。
- **置信度**：高（逻辑可证；具体机型未实测）。

### P2-2 `vkQueuePresentKHR` 的 `VK_ERROR_SURFACE_LOST_KHR` 未走恢复路径，present 失败直接升级为粘性错误

- **证据**：`crates/ui_core/src/render/linux/swapchain.rs:310-315` 只匹配 `Ok(false)`、`Ok(true)` 和 `VK_ERROR_OUT_OF_DATE_KHR`，其余交给 `classify`；`swapchain.rs:337-347` 只把 `VK_ERROR_DEVICE_LOST` 分类为 `DeviceLost`，`VK_ERROR_SURFACE_LOST_KHR` 会变成普通 `EngineError::Swapchain`。对照 `swapchain.rs:270-279`：acquire 路径明确把 `ERROR_SURFACE_LOST_KHR` 映射为 `Acquire::OutOfDate`。
- **问题**：present 收到 surface lost 时没有走“重建 swapchain / 重建 surface”的恢复路径，GPU 线程会带着错误退出；`Renderer::poll` 之后持续返回同一个粘性错误，应用只能退出。Wayland 合成器崩溃/重启、session 切换、显示热插拔都可能触发该错误。
- **影响**：桌面行情应用对窗口消失/合成器重启的恢复能力为零；用户需要重启整个行情客户端。
- **成熟实现对照**：Chrome 的 Vulkan swapchain 在 present 返回 `VK_ERROR_SURFACE_LOST_KHR` 时重建 surface + swapchain；Qt RHI 的 `recreateSwapChain` 也把 `VK_ERROR_SURFACE_LOST_KHR` 视为可恢复错误。
- **建议**：在 `Swapchain::present` 中把 `ERROR_SURFACE_LOST_KHR` 映射为 `Present::OutOfDate`（或新增 `Present::SurfaceLost`），保留本帧的 `FrameRequest`/damage 以便重试；`GpuRenderer` 对同一 `wl_surface` 先尝试 `oldSwapchain` 重建，失败时通过平台层重新创建 `VkSurfaceKHR`；不要把它降级成普通 `Swapchain` 文本错误。
- **置信度**：高（Vulkan 错误路径静态可证；实际触发频率依赖桌面环境）。

### P2-3 `vkCreateSwapchainKHR` 未使用 `minImageExtent` / `maxImageExtent` / `supportedUsageFlags` / `supportedCompositeAlpha`

- **证据**：`crates/ui_core/src/render/linux/swapchain.rs:110-141` 只读取 surface formats；`swapchain.rs:143-151` 查询 capabilities 后只在 `swapchain.rs:176-183` 用 `min_image_count`/`max_image_count`；`swapchain.rs:170-175` 直接把请求尺寸写入 `image_extent`；`swapchain.rs:193` 固定 `COLOR_ATTACHMENT | TRANSFER_DST`；`swapchain.rs:196` 固定 `CompositeAlphaFlagsKHR::OPAQUE`。
- **问题**：`VkSurfaceCapabilitiesKHR` 的 `minImageExtent`/`maxImageExtent` 是 swapchain 创建的合法范围。Wayland 下 `maxImageExtent` 是“最大支持的 surface 尺寸”，大窗口/8K/异常缩放下可能小于请求值；`supportedUsageFlags` 不保证 `TRANSFER_DST` 一定可用；`supportedCompositeAlpha` 只保证至少一个位，不保证 `OPAQUE`。任一不满足时 `vkCreateSwapchainKHR` 返回错误，而当前错误路径会直接终止渲染器。
- **影响**：大尺寸窗口 resize、特殊合成器或部分驱动上首次创建/重建 swapchain 失败；表现为“窗口放大到某个尺寸就黑屏/退出”，且错误文案只会是底层 `create_swapchain_khr`。
- **成熟实现对照**：Qt RHI 会按 `minImageExtent/maxImageExtent` 夹紧并选择 `supportedCompositeAlpha` 中的可用位；Chrome 的 swapchain 创建同样检查 usage/composite alpha 并做降级。
- **建议**：在 `Swapchain::create` 中夹紧 `extent` 到 `[min_image_extent, max_image_extent]`；检查 `supported_usage_flags` 至少包含 `COLOR_ATTACHMENT | TRANSFER_DST`；从 `supported_composite_alpha` 选择 `OPAQUE`，不可用时选 `PRE_MULTIPLIED` 并同步调整 alpha/clear；所有不满足项在初始化阶段给出具体的 `UnsupportedGpu`。
- **置信度**：高（缺失校验静态可证；具体驱动是否触发需实测）。

### P2-4 damage 路径只省栅格化：每帧整面 `vkCmdCopyImage`，且每个 damage 矩形重放全部 draw op

- **证据**：`crates/ui_core/src/render/linux/swapchain.rs:4-5` 明确“无局部 present：每帧整面 `cmd_copy_image`”；`swapchain.rs:350-372` 把 `present_dirty()` 只作为 `VK_KHR_incremental_present` hint；`crates/ui_core/src/render/linux/compositor.rs:908-1011` 的 `copy_to_swapchain` 始终以 `swapchain.extent()` 全宽全高复制；`compositor.rs:877-897` 在 Partial 分支对每个 damage rect 调一次 `draw_ops`，即每个 rect 都把整帧所有 `FrameOp::Instances` 重画一遍（`MAX_DAMAGE_RECTS = 16`，见 `render/damage.rs:17`）。
- **问题**：damage 只缩小了 fragment 的 scissor 覆盖，**没有减少** persist→swapchain 的全屏 copy，也没有减少 draw call/顶点着色；16 个 damage 矩形时最坏是 16 倍 draw op。4K BGRA 每帧 33 MB，60 Hz 约 2 GB/s、144 Hz 约 4.8 GB/s 的纯 copy，叠加 HFT 图表每帧的实例绘制会显著占用显存带宽和 GPU 前端。
- **影响**：窗口越大、refresh 越高、damage 矩形越碎，partial 路径的收益越接近零，甚至比全量更差；这直接对应“高频行情/复杂图表”的核心场景。当前代码把 `incremental_present` 当 hint，但驱动/合成器即使接受 hint，也改变不了应用侧已经付出的 copy 与 draw 成本。
- **成熟实现对照**：Chrome 的 Vulkan/GL compositor 对 damage 做 buffer-age/内容追踪，尽量只更新变化区域；Qt RHI 的 scissor/stencil 批处理也会把多个 damage 合并成少量 draw。若 WSI 不保证 present 后内容（本代码的设计前提），至少应做到：按 damage 包围盒合并 scissor、按 clip/实例区间剔除不相关 draw，而不是每个矩形重放全表。
- **建议**：短期先把 Partial 的 `draw_ops` 改为“一次遍历 ops，按 damage 包围盒/每 op 的 clip 判断是否提交”，并用一个覆盖所有 damage 的 union scissor（或按图元裁剪）减少 draw call；中期评估直接渲染到 acquired swapchain image，并用每 image 的内容/age 记账实现真正的局部 copy；若坚持 persist + 全量 copy，应在文档/性能预算中明确 partial 只省 raster，不能作为大窗口优化卖点。
- **置信度**：高（行为静态可证；带宽数字为按 4K/60/144 的简单推算，需实测确认）。

### P2-5 每个窗口创建独立的 `VkInstance` + `VkDevice` + 队列，多窗口资源成倍复制

- **证据**：`crates/ui_core/src/render/linux/compositor.rs:151-162` 的 `new_for_surface` 每次调用 `VulkanInstance::new`、`Device::new`、`Swapchain::new`；`crates/ui_core/src/render/linux/renderer.rs:108-125` 每个窗口创建 `GpuRenderer` 并启动一条 `kytrix-vulkan` 线程。`crates/ui_core/src/platform/linux/event_loop.rs:4` 目前注明“单窗口 v1”，但 `App::create_window` 支持多窗口（`app.rs:2144`）。
- **问题**：一旦 Linux 平台补上多窗口，每个窗口都会有独立的 instance/device/queue/命令池/descriptor pool/atlas 页/persist；3 张 swapchain image 与 persist 也会按窗口数复制，显存预算和驱动对象数成倍增长。多个 `VkDevice` 之间不能共享 descriptor cache、内存分配器和 GPU 时间线，跟 Chrome（单 GPU 进程/单 device）和 Qt RHI（一个 QRhi/device 服务多窗口）的结构相反。
- **影响**：多显示器行情终端常见 3-4 个窗口，内存/显存占用和驱动开销线性放大；长时间运行更容易 OOM 或触发驱动限制。
- **成熟实现对照**：Chrome 使用单 GPU 进程、单 `VkDevice`，每窗口只持有 swapchain/surface；Qt RHI 通常一个 device 对应多个 window/swapchain。
- **建议**：把 `VkInstance`/`VkDevice`/queue 提升为进程级共享对象（引用计数或 `OnceLock`），每窗口只创建 `VkSurfaceKHR`、`VkSwapchainKHR`、persist 和命令资源；至少在多窗口实现前先重构掉“每 Renderer 一个 Device”的假设。若产品明确只支持单窗口，应在 Linux 平台层显式拒绝第二个窗口而不是静默复制。
- **置信度**：高（代码结构可证；多窗口当前尚未实现，影响是架构推断）。

### P2-6 关闭/替换 swapchain 时只 `vkDeviceWaitIdle` 后销毁旧 swapchain，未证明 presentation engine 已释放图像

- **证据**：`crates/ui_core/src/render/linux/swapchain.rs:15-17` 的模块文档自己写明“`vkDeviceWaitIdle` **不能**证明 presentation engine 已释放图像，因此不作为释放依据”；但 `crates/ui_core/src/render/linux/compositor.rs:1101-1113` 的 `GpuRenderer::drop` 在 `queue_wait_idle` 后直接 `self.retired_swapchains.clear()`，`Swapchain::drop`（`swapchain.rs:324-335`）随即销毁 `render_finished` 信号量和 swapchain。运行时重建路径有延迟销毁，退出路径没有等 presentation 完成。
- **问题**：Vulkan 要求销毁 swapchain 前所有 presentation 操作完成；`vkQueueWaitIdle` 只保证队列提交完成，不保证 Wayland 合成器/驱动已经释放 presentable image。窗口关闭、renderer 替换或合成器较慢时，可能销毁仍被 presentation engine 引用的 buffer/semaphore。
- **影响**：轻则 device lost / 驱动crash，重则在关窗或切换渲染器时触发 use-after-free；与驱动实现相关，lavapipe 上可能完全不复现，真实 GPU/慢合成器上风险更高。
- **成熟实现对照**：Chrome 使用 `VK_KHR_present_wait`/present id（或平台等价的 surface 同步）确认 present 完成；Qt RHI 在销毁 swapchain 前等待渲染/呈现完成并处理平台 surface 生命周期。Wayland 可用 `wl_display.sync`/surface commit 回调确认，或保留旧 swapchain 直到新 surface 提交生效。
- **建议**：把最后一次 present 的完成纳入关闭握手：优先使用 `VK_KHR_present_wait` + `VK_KHR_present_id`，不可用时通过平台层 `wl_display.sync`/frame callback 等待一次；或在 `Drop` 中保留旧 swapchain 到 surface 销毁由驱动清理，不主动销毁 presentation 仍在用的对象。至少在真实 GPU 上做“关闭窗口 ×1000 + 慢合成器”压力测试。
- **置信度**：中（代码与自身注释矛盾是确定的；“真实驱动是否会因此崩”需实机验证）。

### P2-7 实例/裁剪上传缓冲每帧 `vkMapMemory`/`vkUnmapMemory`，与 Windows 的持久映射路径不一致

- **证据**：`crates/ui_core/src/render/linux/compositor.rs:1328-1352` 每帧调用 `map_memory`、两次 `copy_nonoverlapping`、`unmap_memory`；`UploadSlot`（`compositor.rs:1264-1273`）没有持久映射指针。对照 Windows `crates/ui_core/src/render/windows/compositor.rs:1341-1352`、`1359-1372`、`1420-1432`：每个槽创建时 `Map` 一次并保存 `instance_ptr`/`clip_ptr`，之后只做 memcpy（DEVIATIONS F-210）。
- **问题**：`vkMapMemory` 在一些驱动上会做同步/页表操作，尤其非 UMA/带 IOMMU 的独显；每帧 2 次映射（实例+裁剪共用一块分配，但 map 整块）在高频 UI 下会表现为 CPU 尖峰和偶发掉帧。当前 Linux 还要求 `HOST_COHERENT`，不需要每次 flush，但没有利用持久映射的简单优化。
- **影响**：60-144 Hz 行情应用每帧多出 2 次驱动调用；可能不是最大瓶颈，但属于可以零风险消除的固定开销。
- **成熟实现对照**：Windows 后端已按 F-210 采用“构造时 Map 一次、生命周期内只写指针”；Vulkan 官方 best practices 也推荐对每帧更新的 staging/上传缓冲持久映射（`VK_MEMORY_PROPERTY_HOST_COHERENT` 时无需 flush）。
- **建议**：`UploadSlot` 增加 `instance_ptr`/`clip_ptr`，在 `UploadSlot::new` 时 `map_memory(WHOLE_SIZE)` 并保存，扩容时重建；Drop/换槽时 `unmap_memory`；`write` 只做两次 `copy_nonoverlapping`。注意 `Send`/线程归属（GpuRenderer 已在 GPU 线程独占）。
- **置信度**：高（代码可证；实际收益取决于驱动）。

### P3-1 图集页变化时 `queue_wait_idle` 会阻塞整个 GPU 线程与所有在飞帧

- **证据**：`crates/ui_core/src/render/linux/atlas.rs:118-131` 的 `AtlasTextures::prepare` 在检测到 glyph/image `page_change` 后直接 `device.raw().queue_wait_idle(device.queue())`；随后 `atlas.rs:407-423` 的 `recreate` 立即销毁旧 image/view/memory。
- **问题**：图集页从 512² 增长到 1024²/2048²（首次遇到大字符集/CJK），或图片图集首次锁档时，GPU 线程会等待队列完全空闲，期间已经提交的 1-3 帧都无法完成，UI 的 present 被拖住。对 144 Hz 行情 UI，这是一次可感知的启动/首次渲染卡顿。
- **影响**：页变化是低频路径，但恰好发生在“打开新图表/切换字体/DPI 变化”这类用户可感知时刻；与 macOS 的 retire 延迟销毁策略相比，Linux 这里是最重的同步方式。
- **成熟实现对照**：macOS 后端按 F-208 使用资源 retire 队列延迟销毁旧纹理，不需要 `wait_idle`；Qt RHI 的纹理重建通常也通过延迟释放/命令缓冲完成同步。Vulkan 最佳实践是“新资源创建 + 描述符更新后，把旧资源放进按帧 retire，不阻塞队列”。
- **建议**：预先创建新 image/view/memory（不销毁旧的），先 `pipeline.write_atlas` 更新描述符，再把旧 image/view/memory 按当前 `FrameIndex` 进入 retire，等 timeline/fence 完成后释放；如果必须等待，最多等待当前槽 fence 而不是整个 `queue_wait_idle`。
- **置信度**：高（代码可证；卡顿时长需实机量测）。

### P3-2 离屏 `submit_snapshot` 从不清理 `fresh`，导致每个离屏帧都走全量 clear/draw

- **证据**：`crates/ui_core/src/render/linux/compositor.rs:482-484` 的 `submit_snapshot` 只调用 `record_and_submit`、`finish_sync`，没有 `frame.presented()`；`fresh` 只在 `submit_frame` 的 present 成功后清除（`compositor.rs:434`、`render/compositor.rs:163-165`）。`Renderer::publish` 的离屏分支（`renderer.rs:153-155`）每次都用 `FramePlan::new(request.plan, gpu.is_fresh())`，因此 `fresh` 永远为 true。
- **影响**：`Window::enable_offscreen_renderer` 和 `test-support` 的重复快照/离屏动画每次都会 clear 整个 persist 并重画全部 op，`Plan::Partial` 永远不生效；不影响窗口产品路径，但会让离屏基准和快照测试高估绘制成本，也掩盖 partial 路径问题。
- **成熟实现对照**：窗口路径的 `FramePlan::presented` 生命周期是一致的；离屏路径应同样在“首帧成功写入 persist 后”清除 fresh，而不是因为没有 present 就永久不清。
- **建议**：在 `submit_snapshot` 成功后调用 `frame.presented()`（或增加 `mark_fresh_consumed`），并把 `Renderer::publish` 的离屏 `fresh` 参数改为与真实持久目标状态一致；对重复快照的首次帧 clear、后续帧 load。
- **置信度**：高（代码可证；影响仅限离屏路径）。

### P3-3 初始化失败路径大量裸 Vulkan 句柄未清理（无 RAII/局部 guard）

- **证据**：`crates/ui_core/src/render/linux/instance.rs:77-90` 创建 `VkInstance` 后若 `create_wayland_surface` 失败，`?` 返回但 `raw` 无 Drop；`device.rs:273-325` 在 `create_device` 后创建 timeline/fence/semaphore，任一步 `?` 都会泄漏已创建的 device 及前置句柄；`swapchain.rs:200-224` 创建 swapchain 后若 `get_swapchain_images` 或 semaphore 创建失败，swapchain/已创建 semaphore 泄漏；`compositor.rs:1137-1203` 的 `Persist::new` 在 image/view/framebuffer 中途失败会泄漏已创建句柄；`atlas.rs:62-68` 的 `AtlasTextures::new` 第二张页失败会泄漏第一张（`AtlasTexture` 无 Drop，仅 `AtlasTextures` 有 Drop）；`pipeline.rs:68-146`、`compositor.rs:1452-1481` 同样存在分段创建。
- **问题**：没有使用 `ash::vk::Unique*` 或局部 scope guard；任何初始化步骤失败都可能把前面创建的 raw handle 永久留给进程。常规产品路径失败后会退出，但初始化重试、测试多次创建、嵌入其它 host 进程时会累积泄漏并触发 validation 的 object leak。
- **影响**：错误路径资源泄漏，降低启动失败场景的健壮性；不直接影响正常帧。
- **成熟实现对照**：成熟 Rust Vulkan 代码用 `ash::vk::UniqueInstance/UniqueDevice/UniqueSwapchainKHR` 或显式 `Drop` 包装；Qt RHI/Chrome 也有 RAII/cleanup 栈。
- **建议**：把初始化函数改为创建即拥有（`Unique*` 或小型 RAII guard）；或者每个 `?` 前用局部 `Option`/闭包 cleanup；至少在 `VulkanInstance::new`、`Device::new`、`Swapchain::create`、`Persist::new`、`AtlasTextures::new` 这五处补齐。
- **置信度**：高（代码路径可证；仅错误路径）。

### P3-4 没有显存预算/内存压力处理，固定分配在大窗口/多窗口下可能 OOM

- **证据**：`crates/ui_core/src/render/linux/device.rs:277-281` 只取 `get_physical_device_memory_properties`，未查询 `VK_EXT_memory_budget` 或各 heap 的 budget/usage；`compositor.rs:1163-1178` 的 persist 按窗口物理尺寸直接分配 `DEVICE_LOCAL`；`atlas.rs:461-487` 的图集页按 512/1024/2048 档位直接分配。
- **问题**：4K 窗口 persist 约 33 MB，3 张 swapchain image 再加约 100 MB；多窗口 + 2048² 图片图集（16 MB/窗口）+ 字形页会快速接近低端独显/集显的可用预算。当前分配失败只映射成 `AppInit`/`allocate_memory` 文本错误，没有预算判断、没有降低页档/拒绝新窗口/降级到更小 persist 的策略。
- **影响**：多显示器行情终端或同时打开多个图表窗口时，可能出现 `VK_ERROR_OUT_OF_DEVICE_MEMORY` 直接启动失败，或在运行中 resize 时崩溃/黑屏。
- **成熟实现对照**：Chrome 使用 `VK_EXT_memory_budget`/内存压力回调限制 tile/texture 缓存；Qt RHI 有 allocation failure fallback（降级纹理/释放缓存）；至少应在初始化时读取 heap size 并对 persist/atlas 做上限估算。
- **建议**：查询 `VK_EXT_memory_budget`（存在时）；启动时按窗口尺寸估算 persist+swapchain+atlas 峰值，超过预算给明确 `UnsupportedGpu`/`Resource` 错误；图集页档位与窗口数相乘后设上限；运行期分配失败时释放最旧的离屏/图集缓存再重试（若产品允许）。
- **置信度**：高（缺失策略可证；触发条件取决于硬件/窗口规模）。

## 做得好的地方

- **同步原语所有权基本正确**：fence 创建即 `SIGNALED`（`device.rs:304-315`），`wait_slot` 在 acquire 之前执行（`compositor.rs:314-324`），`image_available` 每帧槽一个、`render_finished` 每 swapchain image 一个（`swapchain.rs:60-71`、`211-224`），符合 Khronos frames-in-flight 规则和 F-168 的修复方向。
- **swapchain 重建走 `oldSwapchain` 且旧资源延迟销毁**：`swapchain.rs:82-95`、`compositor.rs:736-781`、`compositor.rs:487-512` 使用 timeline 完成值推进 retire，避免 resize 时 `vkDeviceWaitIdle` 和 `NATIVE_WINDOW_IN_USE_KHR`；这是正确方向。
- **命令缓冲/描述符按帧槽预分配**：`create_commands`（`compositor.rs:1452-1481`）只创建 3 个 command buffer，`Pipeline::write_slot` 按 `slot` 写 3 个 descriptor set（`pipeline.rs:165-193`），没有每帧创建 command pool/pipeline；push constant 固定 112 B，结构清晰。
- **barrier 写点集中且 copy 子资源显式**：`copy_to_swapchain`（`compositor.rs:908-1011`）显式设置 `COLOR_ATTACHMENT_OPTIMAL → TRANSFER_SRC_OPTIMAL → COLOR_ATTACHMENT_OPTIMAL` 和 swapchain `TRANSFER_DST_OPTIMAL → PRESENT_SRC_KHR` 的完整链路；`ImageCopy` 显式给出 `aspect=COLOR`/`layer_count=1`，避免了 `default()` 的非法零值。
- **atlas 上传实现认真**：每 in-flight 槽独立 staging ring（`atlas.rs:526-620`），超过 1 MiB 的整页上传走一次性缓冲并按帧 retire（`atlas.rs:269-284`、`368-376`），逐段 `buffer_offset` 与 `image_offset`（`atlas.rs:319-350`）正确，行打包和 `DirtyRows` 契约有测试。
- **颜色/格式路径正确**：persist 与 swapchain 都选 `*_SRGB`（`swapchain.rs:117-141`、`compositor.rs:58-59`），pipeline 使用预乘 alpha blend（`pipeline.rs:330-338`），`apply_gamma` 交给 sRGB attachment（`common.glsl:131-134`），clear 使用 linear 值——方向正确。
- **错误处理和可观测性**：`DeviceLost` 粘性错误（`device.rs:457-480`）、GPU 线程 `PanicGuard`（`renderer.rs:419-447`）、`FrameGate` 防死锁（`platform/linux/frame_gate.rs`）、GPU 计时 query pool（`timer.rs:25-47`）都比普通手写 Vulkan 完整。

## 存疑 / 需实测

- **partial 帧的 loadOp 依赖是否缺少 `COLOR_ATTACHMENT_READ`**：`pipeline.rs:271-278` 的 `VkSubpassDependency` 只设 `dstAccessMask = COLOR_ATTACHMENT_WRITE`，但 `pass_load` 的 `loadOp=LOAD` 会读取上一帧 persist 内容（`VK_ACCESS_COLOR_ATTACHMENT_READ_BIT`）。如果真实驱动/同步验证把 load 视为 read，partial 帧可能读到未可见的旧数据。建议在 lavapipe + 真实 GPU 打开 synchronization validation 跑 partial 帧；确认后把 `COLOR_ATTACHMENT_READ` 加进 dst access，并补相应 barrier。
- **GPU 计时查询与槽位复用竞争**：`renderer.rs:744` 每轮先 `drain_completed`，随后 `compositor.rs:322-324` `wait_slot` 可能阻塞；如果该槽上一帧的 fence 在 `drain_completed` 之后才完成，`timer.begin`（`compositor.rs:672-674`）会先 reset 查询对，而 `resolve_frame_timer` 还没有读上一帧的 sample。HUD 的 GPU 时间可能取到新帧/部分值。建议在 `wait_slot` 返回后、录制前再调用一次 `drain_completed`/`resolve`，或用每槽独立的 query 双缓冲验证。
- **resize 期间合并的 damage 可能未按新 surface 重新裁剪**：`render/frame.rs` 的 `LatestFrame::publish` 合并旧 plan 到新请求时保留新 `surface`，但 `Plan::merge` 不重新裁剪；窗口缩小时旧 damage 矩形可能超出新 extent。`swapchain::present_rects` 直接把这些矩形传给 `VK_KHR_incremental_present`。建议在 `FramePlan::new` 或 resize 后用新 `surface` 对 `DamageRects` 重新 `intersect`，并用 validation 跑 live resize/partial present。
- **历史验证记录的 `VUID-vkCmdDraw-None-09600`**：已删除的 `design/linux-verification.md` 记录过 snapshot 测试有 22 条该 VUID（demo 0 条，8/8 仍 pass，疑为 validation false positive）。当前 HEAD 需要重新确认：如果在新 lavapipe/validation 版本仍有输出，应把它当独立 issue 而不是忽略；重点检查 persist image 首帧 render pass 的 layout 记录和 offscreen `fresh` 路径（与本报告 P3-2 相关）。
- **多窗口**：Linux `event_loop.rs:4` 明确是单窗口 v1，`App::create_window` 可创建多个窗口但 Linux 运行循环只处理第一个 `id`。因此 P2-5 的“每窗独立 Device”目前不会在多窗口产品中触发；一旦平台层补多窗口，必须同时解决共享 device 和每窗口 surface/swapchain 生命周期。
- **`GpuRenderer::drop` 的 `queue_wait_idle`/viewport 销毁顺序**：`compositor.rs:1092-1113` 先 `viewport.destroy()`、再 `queue_wait_idle`、最后清理 swapchain；在 Wayland 连接已断/合成器卡住时需要实测是否会阻塞或触发协议错误，建议把 viewport 销毁放到 Vulkan teardown 之后、或由平台层统一管理。
