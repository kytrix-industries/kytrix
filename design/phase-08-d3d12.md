# Phase 08 · render 共享层 + Windows D3D12 + readback — 详细设计（v1）

> 依据：`custom-gui-engine-design.md` §2.1 / §2.2 / §2.4 / §2.7 / §2.9 / §2.10、§5.7、§6.1 / §6.4 / §6.6、附录 B；
> `design/phase-02-paint-foundation.md` §8 / §10；`design/phase-06-paint-runtime.md` §7 / §9；
> `design/phase-07-shaders.md`（contract / `ClipEntry` / `RootUniform` / SRV / sRGB）；已落地
> `paint/{instance,pass}.rs`、`render/damage.rs`、`window.rs`、`app.rs`；P01 附录 A.5（windows 0.62 事实）；
> DEVIATIONS F-23 / F-59 / F-61 / F-62 / F-68 / F-69 / S-68 / S-69 / S-71 / S-72。
>
> 本文 §N 指本文件；本轮**只做设计 + 本机 probe**，不写仓库代码、不改 `Cargo.toml`/`Cargo.lock`、不 commit。
> probe 全部产物在 `%TEMP%\kg-p08-d3d12`（仓库外），复现见附录 B。

---

## 0. 结论摘要（先给定死项）

| # | 定死项 | 一句话依据 |
|---|--------|-----------|
| 1 | **共享层零 unsafe、零平台类型**：`render/{device,frame,retire,compositor,timer}.rs` 全是安全 Rust 的纯数据 + 纯函数；`unsafe` 只出现在 `render/windows/**`（每个块带 `// SAFETY:`） | I1/I9；单后端用 `#[cfg(windows)]` 而非 trait/枚举分派 |
| 2 | **D8 用 probe 里已验证的查询路径**；`HighestShaderModel` 必须**先填期望上限**（6_8）再查（本机填 6_0 会真的返回 6_0 → 误杀 RTX 5080） | 本机 probe §2.2；F-73 |
| 3 | **持久 offscreen 是唯一绘制目标**；swapchain back buffer **只作 copy 目的地**（不建 RTV、不挂 scissor 绘制）。每帧：`draw(persist, scissor=damage)` → `CopyTextureRegion(persist→back, box=damage)` → `Present1(dirty=damage)`。**persist 资源格式 = `B8G8R8A8_UNORM`，RTV 视图格式 = `B8G8R8A8_UNORM_SRGB`**（编码一次），swapchain 同为 `*_UNORM`（同格式才能 copy） | 少一遍 draw；不对 swapchain buffer 持 RTV 引用（resize 前提）；probe §2.5 的像素结果 + §2.3 的格式互操作实测 |
| 4 | **Present1 三条实测硬约束**：①swapchain 创建后/每次 `ResizeBuffers` 后**第一次 present 必须全量**（`DirtyRectsCount = 0`），否则 `0x887A0001`；②dirty rect 必须**非空且完全落在 surface 内**；③`pPresentParameters` **不得为 NULL**（本机直接 AV 终止进程） | probe §2.6；主设计 §2.4 的四条约束不含这三条 |
| 5 | **D3D12 没有 `CopySubresourceRegion1`**（那是 D3D11.3 `ID3D11DeviceContext1`）；等价物是 `CopyTextureRegion` + `D3D12_BOX`。区域求交在共享层 `render/compositor.rs` 做完，不引 Win32 `IntersectRect` | windows 0.62.2 源码事实；F-70 |
| 6 | 帧流程 = `begin(等 slot fence → 复位 allocator/list → drain retire) → 上传(实例/图集脏行/root 常量) → draw persist(逐 damage scissor) → copy persist→back → Present1 → signal → (同步)等本帧` | §6；probe §2.5/§2.7 的 ring 实测 |
| 7 | 只有 descriptor table（SRV `t0..t3`）+ 28 DWORD root constant（`b0`）+ **静态采样器** `s0`；根签名 blob **184 B**。**不启用** SM 6.6 `ResourceDescriptorHeap` | probe §2.4（blob 184 B、PSO/DSV 全绿）；S-74（WARP 报 SM 6.8/Tier3，启用会让 CI golden 与真机分叉） |
| 8 | `GpuResourceHandle` = `u64` = `kind:u4 \| generation:u28 \| index:u32`；P08 只产生 `kind=1 (Object)`（`ID3D12Resource`），`ResourceTable` 用 generation 拒绝陈旧句柄 | §8；descriptor 索引在 P08 **静态分区**、不需要回收（S-75） |
| 9 | readback：`GetCopyableFootprints` 的 `RowPitch` 按 256 B 对齐（本机 256×256 BGRA8 → pitch 1024、total 262144）；逐行 `memcpy` 忽略 padding；`Map` 前必须 `SetEventOnCompletion` + `WaitForSingleObject` 等到 fence | probe §2.3/§2.6；§7.7 |
| 10 | `SnapshotImage` 的字节 = 持久 offscreen 的 sRGB 编码 BGRA→RGBA 交换后的**预乘** RGBA8；容差机制（max/avg 通道差 + 超阈像素数）在 P08 落，**阈值数值**仍由 P18/P7 定 | §7.7；主设计 §6.4 |
| 11 | 三平台门禁：Windows 跑 WARP 全绿；非 Windows 只做 `cargo check`（`render/windows` 不编译、`Window.renderer` 字段不存在、`snapshot` 恒 `NoRenderer`） | §9；主设计 §6.6 / I7 |
| 12 | 新增 windows feature **`Win32_Security`**（`CreateEventW` 在 0.62.2 被该 feature 门控；缺了是 `E0425`）。只改 feature 列表，**不加依赖、`Cargo.lock` 零变化** | 本机最小复现 §2.10；F-79 |

---

## 1. 范围、文件与非目标

### 1.1 本阶段交付（P08 实现期落地的文件）

```text
crates/kytrix_gui/src/
├── render.rs                          # 私有模块根：只 mod 声明 + pub(crate) use（无类型定义）
├── render/
│   ├── device.rs                      # GpuCaps + D8 基线（纯数据 + 纯函数）
│   ├── frame.rs                       # MAX_FRAMES_IN_FLIGHT / InFlightRing / slot（FrameIndex 留在 window.rs）
│   ├── retire.rs                      # RetireQueue + GpuResourceHandle 编码
│   ├── compositor.rs                  # damage → Plan（Full / Partial(固定数组)），纯函数
│   ├── timer.rs                       # Section / GpuTimerSample / FrameTimings
│   ├── atlas.rs                       # 已落地（P05）：AtlasAllocator / DirtyRows
│   ├── damage.rs                      # 已落地（P06）：DamageAccumulator
│   └── windows.rs                     # 后端模块根（只 mod 声明 + pub(crate) use Renderer）
│       └── windows/
│           ├── device.rs              # ID3D12Device / 适配器选择 / ResourceTable / DRED / lost 检测
│           ├── queue.rs               # 命令队列 + 每 in-flight allocator/list + fence + retire 排空
│           ├── swapchain.rs           # FLIP_SEQUENTIAL swapchain / Present1 / resize / composition 变体
│           ├── pipeline.rs            # 根签名 / quad PSO / geometry PSO / SRV 堆 / GeometryRegistry
│           ├── atlas.rs               # R8 / BGRA8_SRGB 纹理、脏行上传、generation 重建
│           ├── compositor.rs          # 帧编排（begin→upload→draw→copy→present→signal）+ Renderer
│           ├── readback.rs            # offscreen→readback heap→CPU（快照路径）
│           └── timer.rs               # timestamp query heap（GPU 段）
```

**修改**：`window.rs`（`renderer: Option<Renderer>` + `commit_frame` 变 `Result`）、`app.rs`（转发 GPU 错误）、
`error.rs`（`GeometryRegister`）、`test_support/snapshot.rs`（`SnapshotError::Gpu`）、`test_support/app.rs`（WARP 快照）、
`paint/instance.rs`（上传访问器 + `ClipChain → ClipEntry`）、`Cargo.toml`（windows features 加 `Win32_Security`）。

### 1.2 明确的非目标

| 不做 | 归属 |
|------|------|
| macOS Metal / Linux Vulkan 后端 | P11 / P17 |
| Win32 平台层（窗口创建、消息循环、DPI、IME、输入） | P09 / P12 |
| `GeometryStore` 的**公开**注册 API（`App::register_geometry`）与 VBO 生命周期管理 | P09（P08 只有 `pub(crate)` 的 `GeometryRegistry` + test-only 注册，见 §5.4） |
| SM 6.6 `ResourceDescriptorHeap` / Tier3 加速路径 | P09+（S-74） |
| 像素 golden 文件、diff 工具、CI 设备矩阵 | P18（P08 只落机制与容差参数结构） |
| 多线程录制 / 异步 present / 事件循环 | P09（P08 同步提交 + 同步等待） |
| 新依赖（`log`、`anyhow`…） | 不加；诊断走 `GpuCaps::describe()` + `EngineError` |

### 1.3 与已落地代码的关系（事实基线）

| 已落地 | P08 如何消费 |
|--------|-------------|
| `paint::instance::{InstanceBuffer, GpuPrimitive, FrameOp, ClipChain}` | `as_bytes()` 上传实例；新增 `clips()/geometry()/ops()` 与 `ClipChain::gpu_entry()`（§7.3） |
| `render::damage::DamageAccumulator` | `rects()` → `render::compositor::plan(...)`；不改 damage 算法 |
| `window::{FrameIndex, FrameScheduler, Window}` | `FrameIndex::get() % 3` 取槽；`Window.renderer` 与提交点 |
| `render/shaders/{quad,geometry}.hlsl`（P07） | `include_bytes!` 加载；PSO 契约按 P07 §2.9 逐字 |
| `error::EngineError`（11 变体） | 复用 `UnsupportedGpu` / `DeviceLost` / `Swapchain` / `ShaderLoad` / `AtlasExhausted`；新增 `GeometryRegister` |
| `test_support::{TestApp, SnapshotCase}` | `snapshot` 由 `NoRenderer` 改为 Windows 上真渲染 |

---

## 2. probe（本机，2026-11-12）

> 目录 `%TEMP%\kg-p08-d3d12`（仓库外、独立 `target`）。依赖 `windows 0.62.2`（与 workspace 同版本，`Cargo.lock` 不参与）。
> 着色器直接编译仓库里**已落地**的 `quad.hlsl`（`-T vs_6_2/ps_6_2 -WX`），因此 PSO/根签名结论对真实产物成立。

### 2.1 环境

| 项 | 值 |
|---|---|
| OS / 会话 | Windows，非交互式 agent 会话（无可见桌面；DWM 存在） |
| GPU | `NVIDIA GeForce RTX 5080`（vendor 0x10DE / device 0x2C02 / 15977 MiB），DXGI 枚举到 **2 份**（同 LUID 的两个输出组）+ `Microsoft Basic Render Driver` |
| WARP | `IDXGIFactory4::EnumWarpAdapter` 可用 |
| dxc | `Windows Kits\10\bin\10.0.26100.0\x64\dxc.exe`；`quad.vs` 5600 B、`quad.ps` 7864 B，magic `DXBC` |
| D3D12 debug layer | **不可用**：`D3D12GetDebugInterface` → `0x887A002D`（`DXGI_ERROR_SDK_COMPONENT_MISSING`）→ 未安装 Graphics Tools |
| DRED | `D3D12GetDebugInterface::<ID3D12DeviceRemovedExtendedDataSettings>` **可用**，`SetAutoBreadcrumbsEnablement(FORCED_ON)` + `SetPageFaultEnablement(FORCED_ON)` 在建设备**之前**生效 |

### 2.2 D8 检测实测（逐项）

```text
hardware caps: FL=12_2 SM=6_8 binding_tier=3 heap_tier=2 tiled=4 min_precision=2
               native16=true root_sig=1 max_sampler_heap=4080 baseline_ok=true
warp caps:     FL=12_1 SM=6_8 binding_tier=3 heap_tier=2 tiled=3 min_precision=3
               native16=true root_sig=1 max_sampler_heap=2097152 baseline_ok=true
```

| # | 检测 | 实测结论 |
|---|------|---------|
| 1 | `D3D12CreateDevice(adapter, FL_11_0)` | 硬件与 WARP 都成功（11_0 起步再查上限，与主设计 §2.2 #1 一致） |
| 2 | `D3D12_FEATURE_FEATURE_LEVELS` | 硬件 **12_2**、WARP **12_1**（≥ 12_0 硬基线成立） |
| 3 | `D3D12_FEATURE_SHADER_MODEL` | **必须把 `HighestShaderModel` 预填 6_8**：填 6_0 时同一设备返回 6_0（probe 首轮实际踩到，误判 `baseline_ok=false`）；填 6_8 后硬件/WARP 都返回 **6_8** → **F-73** |
| 4 | `D3D12_FEATURE_D3D12_OPTIONS` | `ResourceBindingTier = 3`（≥2 ✓）、`ResourceHeapTier = 2`、`TiledResourcesTier = 4`（WARP 3）、`MinPrecisionSupport = 2`（WARP 3） |
| 5 | `OPTIONS4.Native16BitShaderOpsSupported` | 两者 `true`（不加门槛，只记录） |
| 6 | `D3D12_FEATURE_ROOT_SIGNATURE` / `OPTIONS19.MaxSamplerDescriptorHeapSize` | `HighestVersion = 1`（V1_0）；`MaxSamplerDescriptorHeapSize` 硬件 **4080** / WARP **2097152** |
| 7 | FLIP_SEQUENTIAL + BufferCount ≥ 2 | 创建成功；`GetDesc1` 回读 `format=DXGI_FORMAT(87)=B8G8R8A8_UNORM`、`buffercount=3`、`SwapEffect=3`、`Scaling=0(STRETCH)`、`flags=0` |
| 8 | 可选 SM6.6 / Tier3 | **两者都满足**（硬件 Tier3 + SM6.8，WARP 同）→ 若启用 `ResourceDescriptorHeap`，CI 的 WARP golden 会走与真机不同的路径（主设计 §6.4 的同一条警告）→ 决定**不启用**（S-74） |

### 2.3 offscreen clear + readback（sRGB 语义与 footprint，实测）

```rust
// 骨架（probe 中可运行；省略了 helper 与错误映射）
// 注：这一段用「资源本身就是 sRGB 格式」验证编码语义；引擎最终采用表里第 5 行的
// 「UNORM 资源 + SRGB RTV 视图」（原因见 §5.3），两者编码语义相同（clear(0.5) → 188）。
let tex = create_texture(&device, 256, 256, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
                         /*RT*/ true, D3D12_RESOURCE_STATE_RENDER_TARGET)?;
let heap: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV, NumDescriptors: 8,
    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE, NodeMask: 0 })?;
let rtv = heap.GetCPUDescriptorHandleForHeapStart();
device.CreateRenderTargetView(&tex, None, rtv);
list.ClearRenderTargetView(rtv, &[0.25, 0.5, 0.75, 1.0], None);

// 读回：footprint 由 D3D12 计算，不能手算 pitch
let mut placed = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
let (mut rows, mut row_size, mut total) = (0u32, 0u64, 0u64);
device.GetCopyableFootprints(&desc, 0, 1, 0, Some(&mut placed),
                             Some(&mut rows), Some(&mut row_size), Some(&mut total));
// placed.Footprint.RowPitch = 1024（256×4 已满足 256 B 对齐）；total = 262144
```

| 实测 | 结果 |
|---|---|
| sRGB 目标的 clear 值 | `clear(0.25,0.5,0.75)` 读回 **137 / 188 / 225**，与 `srgb_encode(线性值)` **逐字节相等** → **clear 颜色按线性值给出、由目标格式编码一次**（与 P07 §2.3「唯一编码点」一致；这是 golden 期望值可直接计算的依据） |
| footprint | `256x256 → row_pitch=1024 rows=256 row_size=1024 total=262144`；`D3D12_TEXTURE_DATA_PITCH_ALIGNMENT = 256` |
| 描述符步长 | `CBV_SRV_UAV = 32`、`RTV = 32`、sampler = 32（硬件） |
| `CreateCommittedResource` | 非 RT 资源传 `Some(&D3D12_CLEAR_VALUE)` → `E_INVALIDARG (0x80070057)`；**只有带 `ALLOW_RENDER_TARGET` 的资源才传优化清除值** |
| **sRGB 视图互操作** | `B8G8R8A8_UNORM` 资源 + `CreateRenderTargetView(Format = B8G8R8A8_UNORM_SRGB)`：`clear(0.5)` 读回 **188**（= sRGB 编码值）⇒ **资源格式 UNORM、视图格式 SRGB 就能在一次写入里完成编码** |
| **sRGB swapchain** | `CreateSwapChainForHwnd` 用 `B8G8R8A8_UNORM_SRGB` + FLIP_SEQUENTIAL → **`0x887A0001`**（本机驱动/OS 不接受 sRGB flip swapchain）⇒ swapchain 必须 `*_UNORM` |
| 结论 | 读回路径（RT→COPY_SOURCE→`CopyTextureRegion`→READBACK buffer→`Map`）在硬件上像素级正确；`persist(UNORM) → back(UNORM)` 的 copy 同格式合法，编码由 persist 的 sRGB **视图**完成一次 |

### 2.4 uber-quad：根签名 / PSO / 实例 SRV（用真实 `quad.hlsl`）

```rust
// 根签名：1 个 28-DWORD root constant（b0）+ 1 张 SRV 表（t0..t3）+ 1 个静态采样器（s0）
let ranges = [D3D12_DESCRIPTOR_RANGE { RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
    NumDescriptors: 4, BaseShaderRegister: 0, RegisterSpace: 0,
    OffsetInDescriptorsFromTableStart: 0 }];
let mut params = [D3D12_ROOT_PARAMETER::default(); 2];
params[0].ParameterType = D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS;
params[0].Anonymous = D3D12_ROOT_PARAMETER_0 { Constants: D3D12_ROOT_CONSTANTS {
    ShaderRegister: 0, RegisterSpace: 0, Num32BitValues: 28 } };
params[1].ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
params[1].Anonymous = D3D12_ROOT_PARAMETER_0 { DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
    NumDescriptorRanges: 1, pDescriptorRanges: ranges.as_ptr() } };
// 静态采样器：FILTER_MIN_MAG_MIP_LINEAR / CLAMP / COMPARISON_NEVER / MaxLOD 0 / s0 / PIXEL
D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1_0, &mut blob, Some(&mut err))?;
let root: ID3D12RootSignature = device.CreateRootSignature(0, blob_bytes)?;   // blob = 184 B
```

| 实测 | 结果 |
|---|---|
| 根签名 blob | **184 B**（`CreateRootSignature` 成功） |
| PSO（`PrimitiveTopologyType = TRIANGLE`，`TriangleStrip` + 4 顶点 + `CullMode NONE` + 无输入布局 + `IBStripCutValue DISABLED` + `NumRenderTargets 1` + `RTVFormats[0] = B8G8R8A8_UNORM_SRGB` + `DSVFormat UNKNOWN` + `SampleDesc{1,0}`） | 成功；`Center=(32,32)` = `[255,0,0]`、`(22,22)` = 红、`(42,42)` = 红、`(4,4)` = 黑 ⇒ **整块 32×32 quad 全部光栅化** |
| 同一 PSO 改用 **`TRIANGLELIST`** 的负例 | `Center=(32,32)` = **黑**、`(22,22)` = 红、`(42,42)` = 黑 ⇒ 只画出 strip 的前 3 个顶点组成的三角形 ⇒ **P07 检查单 §7.1-1b 的坑在本机复现** |
| 实例 SRV | `D3D12_SRV_DIMENSION_BUFFER` + `StructureByteStride = 128` + `NumElements = 1`；`GpuPrimitive` 第 1 项（红色 `color0`、`kind=QUAD`、`flags=OPAQUE`、`params0.w=1`）被 FS 正确解码 |
| `SetGraphicsRoot32BitConstants(0, 28, uniform)` | 112 B `RootUniform`（`frame.viewport = (256,256,2/256,2/256)`）驱动 `to_ndc`，Y 方向正确（与 P07 §2.8 一致） |
| premultiplied 混合（`ONE / INV_SRC_ALPHA`，RGBA 写掩码 0x0F） | 不透明红叠加在黑底上得到 `[255,0,0]`（sRGB 目标 → 编码一次） |

### 2.5 持久 offscreen + 逐 damage scissor + 前帧拷贝（机制实测）

```text
frame1: persist ← clear(黑) + draw(red quad @ (16,16,32,32), scissor = 整面)
        back    ← CopyResource(persist)                       // 全量
frame2: damage = (128,128,48,32)
        back    ← CopyTextureRegion(dst=back(damage.left,top),
                                    src=persist, srcBox=damage)  // 前帧拷贝（只拷 damage）
        back    ← draw(blue quad @ (128,128,48,32), scissor = damage)
```

| 实测 | 结果 |
|---|---|
| frame1 读回 | `(30,30) = [255,0,0]` |
| frame2 读回 | `(140,140) = [0,0,255]`（新蓝色已画）、`(30,30) = [255,0,0]`（**damage 外像素被前帧拷贝保住**） |
| 结论 | `CopyTextureRegion` + `D3D12_BOX` + `RSSetScissorRects(RECT=damage)` 的组合行为与主设计 §2.4 的意图一致；`D3D12_BOX{left,top,front=0,right,bottom,back=1}` 是唯一需要的区域表达，**没有 `CopySubresourceRegion1`** |

### 2.6 swapchain + `Present1` 矩阵（隐藏 HWND）

`CreateSwapChainForHwnd(queue, hwnd, {256×256, B8G8R8A8_UNORM, BufferCount 3, FLIP_SEQUENTIAL, STRETCH, IGNORE})` 成功，
`GetDesc1` 回读与请求一致。逐条调用结果：

| 调用 | 结果 |
|---|---|
| `Present1(sync=1, 2 dirty rects)` 作为**第一次** present | **`0x887A0001 DXGI_ERROR_INVALID_CALL`** |
| 同上第二次（仍未做过全量 present） | 同样 `0x887A0001` |
| `Present(sync=1)` 全量 | `S_OK` |
| 之后 `Present1(sync=1, 2 dirty rects)` | **`S_OK`** |
| `Present1(sync=1, 零填充结构)` | `S_OK`（= 全量） |
| `Present1(sync=1, scroll rect + offset)` | `S_OK` |
| `Present1(sync=1, dirty + scroll 同时)` | `S_OK` |
| `Present1(sync=1, 只有 pScrollOffset 没有 pScrollRect)` | `S_OK`（**DXGI 不校验**，引擎必须自己保证成对） |
| `Present1(sync=1, dirty rect = 320×200 on 256×256)` | `0x887A0001`（越界被拒） |
| `Present1(sync=1, dirty rect = 0×0)` | `0x887A0001`（空矩形被拒） |
| `Present1(sync=1, pPresentParameters = NULL)` | **进程直接终止**（DXGI 解引用空指针，无 HRESULT 返回） |
| composition swapchain（无 HWND）：先 `Present(full)` 再 `Present1(dirty)` | `S_OK`（首帧规则同样适用） |
| `Present(sync=0, 2 dirty rects)` | `S_OK` |

**由此得出 §0 #4 的三条约束**，并写进 `swapchain.rs` 的状态机（`fresh: bool`，第一次 present 强制全量）。
第 4 条（N-buffer 陈旧）在本设计的结构下由「persist 是唯一真值 + 每次 present 前按 damage 拷贝」覆盖（见 §6.3）。

> **格式**：`B8G8R8A8_UNORM_SRGB` 的 flip swapchain **创建即失败**（`0x887A0001`）；swapchain 必须 `*_UNORM`，sRGB 编码改由 persist 的
> **RTV 视图**完成（§2.3 的互操作实测）→ F-80。
>
> **MSAA swapchain**：`Count=4 + DISCARD + BufferCount=2` 在 `CreateSwapChainForHwnd` 阶段被拒（`0x887A0001`），
> 与「flip model 只支持 `Count = 1`」一致 ⇒ 主设计 §2.4 约束 3 的「MSAA swapchain 不支持 dirty/scroll」在 D3D12 上
> 根本走不到（创建即失败），**不是**运行时分支。

### 2.7 resize（**本机受限**，最小复现与替代）

| 场景 | 结果 |
|---|---|
| **从未 present / 未取过 back buffer** 的 flip swapchain：`ResizeBuffers(0,0,0,UNKNOWN,0)` 或 `(3,160,120,UNORM,0)` | **`S_OK`** |
| 已 present 过的 swapchain（drop back buffer + drop RTV 堆 + `list.Reset` + `PeekMessage` 泵消息 后） | `0x887A0001` |
| 带 `FRAME_LATENCY_WAITABLE_OBJECT`（`SetMaximumFrameLatency(1)`），`Present` 后 `WaitForSingleObject(waitable)` 再 resize | `0x887A0001` |
| 失败后 `GetDeviceRemovedReason` | `S_OK`（设备本身没丢） |
| 失败后再 `CommandAllocator::Reset` | 本机出现 `E_FAIL (0x80004005)`（**一次**；不能假设失败可重试） |

**最小复现**：`CreateSwapChainForHwnd`（隐藏 `WS_POPUP`，256×256，FLIP_SEQUENTIAL，BufferCount 3）→ `GetBuffer(0)` →
`ClearRenderTargetView` → `Present(1,0)` → `ResizeBuffers(0,0,0,DXGI_FORMAT_UNKNOWN,0)`。

**替代/处置**：本机是**非交互会话 + 隐藏窗口**（DWM 不消费 flip 队列），无法验证「present 之后再 resize」。P08 的定案：
①resize 的**纯逻辑**（何时 full、何时重建 persist、绝不重建实例容量）用单测覆盖；②`ResizeBuffers` 的调用顺序
（`wait_idle` → 释放所有 back buffer 引用与承载 RTV 的堆 → `list.Reset` → `ResizeBuffers(0,0,0,UNKNOWN,0)`）按 probe 失败路径反推；
③**P09 必须在可见窗口 + 真实桌面会话**上补一条 resize 实机验证（进 §11 R-5 与 V-55）；
④`ResizeBuffers` 失败一律 `EngineError::Swapchain { detail }` **不重试**（失败会污染 allocator）。

### 2.8 in-flight ring / GPU timestamp / device lost + DRED

| 项 | 实测 |
|---|---|
| ring（3 allocator + 3 list，8 帧） | 每帧 `wait_for(slot_value)` → `allocator.Reset` → 录制 → `ExecuteCommandLists` → `Signal`；`GetCompletedValue` 单调到 19；**等待时间 0 µs**（同步提交下自然成立），无 `reset-while-in-flight` 错误 |
| `GetTimestampFrequency` | **1_000_000_000**（1 tick = 1 ns） |
| 64 次 `ClearRenderTargetView` 的 timestamp 差 | **10.1–10.8 µs**（同一帧内 `EndQuery` 两次 + `ResolveQueryData` 到 READBACK buffer） |
| `ID3D12Device5::RemoveDevice` | 之后 `GetDeviceRemovedReason` → `0x887A0005 DXGI_ERROR_DEVICE_REMOVED`（可读消息） |
| DRED | `device.cast::<ID3D12DeviceRemovedExtendedData>()` 成功；`GetAutoBreadcrumbsOutput()` 返回 `S_OK`（未做过 GPU 工作的设备 head 为 null） |
| device 已丢后的 `Present1` | `0x887A0005`（**不是** `INVALID_CALL`）——提交路径用 `GetDeviceRemovedReason` 区分「设备丢失」与「参数非法」 |

### 2.9 `windows 0.62.2` API 事实（probe 编译期逐条核对，P01 A.5 的补充）

| 事实 | 影响 |
|---|---|
| `Present1(...) -> HRESULT`（不是 `Result`）；`ResizeBuffers(...) -> Result<()>`；`Present(...) -> HRESULT` | 调用点必须分别处理 |
| `D3D12GetDebugInterface<T>(result__: *mut Option<T>) -> Result<()>` | 用 out 参数，不能 `::<T>()?` |
| `CreateQueryHeap<T>(pdesc, result__) -> Result<()>` | 同上 |
| `RemoveDevice` 只在 **`ID3D12Device5`** 上 | 必须 `cast::<ID3D12Device5>()` |
| `ID3D12Fence::SetEventOnCompletion` 需要 **`HANDLE`**（`CreateEventW`） | 见 §2.10 的 feature 门控 |
| `CreateCommittedResource(..., poptimizedclearvalue: Option<*const D3D12_CLEAR_VALUE>, result__: *mut Option<T>)` | RT 之外传 `None` |
| `GetCopyableFootprints(&desc, first, num, base, Option<*mut placed>, Option<*mut rows>, Option<*mut row_size>, Option<*mut total>)` | 参数全是裸指针 out |
| `D3D12_CPU_DESCRIPTOR_HANDLE { ptr: usize }` / `D3D12_GPU_DESCRIPTOR_HANDLE { ptr: u64 }` | 偏移靠 `GetDescriptorHandleIncrementSize` 手算 |
| `CreateRenderTargetView` / `CreateShaderResourceView` / `CreateSampler` 返回 **`()`**（不报错） | 失败只能在后续使用（clear/draw/readback）暴露；必须靠 WARP 像素断言守住 |
| `D3D12_RESOURCE_BARRIER { Type, Flags, Anonymous: union{Transition: ManuallyDrop<..>} }` | 构造要 `ManuallyDrop` |
| `ID3D12GraphicsCommandList::SetDescriptorHeaps(&[Option<ID3D12DescriptorHeap>])` / `RSSetScissorRects(&[RECT])` / `OMSetRenderTargets(n, Option<*const handle>, bool, Option<*const handle>)` | 切片/裸指针混合，写代码时注意 |
| `WNDCLASSW`、`CreateWindowExW` 被 **`Win32_Graphics_Gdi`** 门控（`hbrBackground: HBRUSH`） | kytrix_gui 已有该 feature ✓ |
| **`CreateEventW` 被 `Win32_Security` 门控** | kytrix_gui **缺**该 feature → F-79 |
| `CopySubresourceRegion1` **不存在**（`ID3D12GraphicsCommandList1` 只有 AtomicCopy/UINT64、OMSetDepthBounds、SetSamplePositions、ResolveSubresourceRegion、SetViewInstanceMask） | F-70 |

### 2.10 不可行 / 受限项与最小复现

| 项 | 最小复现 | 替代 |
|---|---|---|
| D3D12 debug layer | `D3D12GetDebugInterface::<ID3D12Debug>` → `0x887A002D` | DRED + WARP 像素断言；debug layer **不得**成为启动前提（缺 SDK 组件就失败会杀掉所有装机） |
| present 之后的 `ResizeBuffers` | §2.7 的 5 步 | 纯逻辑单测 + P09 实机验证（V-55）；失败即致命 |
| `Present1(NULL)` | 直接调用 → 进程终止 | 永远传零填充结构（`DirtyRectsCount = 0`） |
| MSAA swapchain | `Count=4 + DISCARD` 创建 → `0x887A0001` | 不需要运行时分支（flip 只支持 Count 1） |
| `Win32_Security` 缺失 | `CreateEventW` → `error[E0425]: cannot find function ... not found in windows::Win32::System::Threading` | 加 feature（F-79） |
| D8 失败路径 | 本机两设备都满足基线，**无法自然失败** | 单测注入 `GpuCaps` 字段（§10） |

---

## 3. 模块结构与边界

### 3.1 文件树与非 test 消费者（dead_code 审计表）

`render.rs`（模块根，**只**允许 `mod` / `pub(crate) mod` / `pub(crate) use` 行）：

```rust
//! 渲染共享层（crate 私有；应用层不可见，I9）。
pub(crate) mod atlas;
pub(crate) mod compositor;
pub(crate) mod damage;
pub(crate) mod device;
pub(crate) mod frame;
pub(crate) mod retire;
pub(crate) mod timer;

#[cfg(windows)]
pub(crate) mod windows;
#[cfg(windows)]
pub(crate) use windows as backend;

pub(crate) use atlas::{Allocation, AtlasAllocator, AtlasSet, DirtyRows};
#[cfg(windows)]
pub(crate) use windows::compositor::Renderer;
```

| item | 定义 | 非 test 消费者 |
|------|------|----------------|
| `GpuCaps` / `D8_REQUIRED` / `check` | `render/device.rs` | `windows/device.rs::Device::new`（建完设备即校验）；`describe()` 被 `UnsupportedGpu.actual` 消费 |
| `MAX_FRAMES_IN_FLIGHT` / `InFlightRing` / `FrameSlot` | `render/frame.rs` | `windows/queue.rs::Queue` |
| `RetireQueue` / `GpuResourceHandle` | `render/retire.rs` | `windows/queue.rs`（`drain`）+ `windows/device.rs`（`ResourceTable::release`） |
| `Plan` / `plan` / `DamageRects` | `render/compositor.rs` | `windows/compositor.rs::Renderer::submit` |
| `Section` / `GpuTimerSample` / `FrameTimings` | `render/timer.rs` | `windows/compositor.rs`（CPU 段）+ `windows/timer.rs`（GPU 段） |
| `Renderer` | `windows/compositor.rs` | `window.rs::Window::renderer` + `test_support/app.rs::snapshot` |
| `ResourceTable` / `Device` / `Queue` / `Swapchain` / `Pipeline` / `GeometryRegistry` / `AtlasTextures` / `Readback` / `GpuTimer` | `windows/*` | 同文件内的 `Renderer`（唯一入口），无跨模块泄漏 |

**可见性纪律**：`render/` 整体 `mod render;`（`lib.rs` 现状）；共享层里 `pub(crate)` 的每一项都必须在上表里有一个**非 test** 消费者
（`cargo clippy --all-targets -D warnings` 的 `dead_code` 门禁）；`render/windows/**` 内部私有项不进上表（同模块内消费即可）。

### 3.2 无 trait / 无枚举分派（I1）

- **没有** `trait RenderBackend`、没有 `Box<dyn Device>`、没有 `enum Backend { Windows, Mac, Linux }`。
- 单一实现 + 编译期选择：`#[cfg(windows)] pub(crate) use windows as backend;`。macOS/Linux 在 P11/P17 追加**同形**模块，`Window` 侧
  通过 `#[cfg]` 选择字段类型（P08 只有 Windows 分支）。
- `render/` 对 `paint/`、`math/` 的依赖是**单向**的（`paint → render` 不存在）；`windows/**` 可以读 `paint::instance::*`（数据契约），
  但**不得**把 D3D12 类型写进 `paint/`（I2/I9）。

### 3.3 `Window` / `App` 的接线（签名变更，逐字）

```rust
// window.rs
#[cfg(windows)]
use crate::render::Renderer;

pub struct Window {
    // ... 既有字段
    /// GPU 渲染器。`None` = headless（I7 的默认；测试与无 GPU 构建）。
    #[cfg(windows)]
    renderer: Option<Renderer>,
}

impl Window {
    /// 建窗时挂渲染器（P09 的窗口层调用；headless 保持 `None`）。
    #[cfg(windows)]
    pub(crate) fn set_renderer(&mut self, renderer: Renderer);

    /// 提交（GPU 存在时同一函数里完成 `begin/upload/draw/present`）。
    pub(crate) fn commit_frame(&mut self, ticket: FrameTicket, report: PaintReport)
        -> Result<FrameOutcome, EngineError>;   // ← P06 的 `FrameOutcome` 改为 `Result`

    /// 最近一帧的编码结果（`Renderer` 的上传源；`instance_bytes()` 内部改为调它）。
    pub(crate) fn instance_buffer(&self) -> &crate::paint::instance::InstanceBuffer;
}
```

`App::frame` 的 `Some(Ok(...))` / `Some(Err(...))` 分支不变（`commit_frame` 的 `Err` 直接冒泡）；`Window::frame()` 同样。
`set_size` / `set_scale` 继续只置 `full_damage_pending`，**不**碰 renderer；真正的 resize 在下一次 submit 的 `begin` 里
（`surface_size != 期望物理尺寸` 时执行，见 §6.2）。

---

## 4. 共享层规格（零 unsafe、可单测）

### 4.1 `render/device.rs` — `GpuCaps` 与 D8 基线

```rust
/// D8 硬基线（错误文案与安装器共用同一份；主设计 §2.2）。
pub(crate) const D8_REQUIRED: &str =
    "FL 12_0 / SM 6.2 / ResourceBindingTier >= 2 / FLIP_SEQUENTIAL / BufferCount >= 2";

/// 启动时一次性采集的能力快照（含诊断字段）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GpuCaps {
    pub(crate) adapter: SmolStr,            // DXGI_ADAPTER_DESC1.Description
    pub(crate) vendor_id: u32,
    pub(crate) device_id: u32,
    pub(crate) dedicated_vram_mib: u64,
    pub(crate) feature_level: i32,         // D3D_FEATURE_LEVEL.0
    pub(crate) shader_model: i32,          // D3D_SHADER_MODEL.0（预填 6_8 查询后的值）
    pub(crate) resource_binding_tier: i32,
    pub(crate) resource_heap_tier: i32,
    pub(crate) tiled_resources_tier: i32,
    pub(crate) min_precision_support: i32,
    pub(crate) native_16bit_shader_ops: bool,
    pub(crate) root_signature_max: i32,
    pub(crate) max_sampler_descriptor_heap: u32,
    pub(crate) warp: bool,                 // test/CI 设备标记（诊断用，不参与基线）
}

impl GpuCaps {
    /// 硬基线三项：FL >= 12_0、SM >= 6.2、BindingTier >= 2。
    pub(crate) fn baseline_ok(&self) -> bool;
    /// `actual` 的**稳定**格式（只含三项基线值，便于测试断言与用户比对）。
    /// 形如 `"FL 11_0 / SM 6_0 / BindingTier 1"`。
    pub(crate) fn actual(&self) -> SmolStr;
    /// 全字段诊断行（日志/A-B 对照；不含时间戳）。
    /// `adapter="…" vendor=0x10DE device=0x2C02 vram=15977MiB FL=12_2 SM=6_8 binding=3 …`
    pub(crate) fn describe(&self) -> SmolStr;
    /// 唯一校验入口：失败 → `EngineError::UnsupportedGpu { os: "windows", required, actual }`。
    pub(crate) fn check(&self) -> Result<(), EngineError>;
}
```

- **不做**任何 I/O 与日志（`log` 不是 kytrix_gui 依赖，P08 不新增依赖）；诊断通过 `describe()` 交给 P09 的日志层。
- `fl_name(i32) -> &'static str` / `sm_name(i32)`：把 `.0` 数值映射成 `"12_0"` / `"6_2"`（**只**在这一个地方转换，测试覆盖 11_0/11_1/12_0/12_1/12_2 与 6_0…6_9）。

### 4.2 `render/frame.rs` — in-flight

```rust
/// 主设计 §2.4 的 N；同步提交下实际最多 1 帧在飞，但 ring 仍按 3 组织（P09 异步化不改结构）。
pub(crate) const MAX_FRAMES_IN_FLIGHT: usize = 3;

/// `FrameIndex` 仍在 `window.rs`（P06 公开 API `Window::frame_index()` 的返回类型，F-74）。
pub(crate) fn slot(index: FrameIndex) -> usize {
    debug_assert!(index.get() >= 1, "FrameIndex 0 从未提交");
    (index.get() as usize) % MAX_FRAMES_IN_FLIGHT
}

/// 一帧要用的槽位与 fence 值（`Queue::begin` 的唯一输入）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameSlot {
    pub(crate) index: usize,
    /// 进入本帧前必须等待到达的 fence 值（0 = 该槽从未使用，无需等待）。
    pub(crate) wait_value: u64,
    /// 本帧提交后 signal 的值（= `next_value`）。
    pub(crate) signal_value: u64,
}

pub(crate) struct InFlightRing {
    slot_values: [u64; MAX_FRAMES_IN_FLIGHT],
    next_value: u64,
}

impl InFlightRing {
    pub(crate) const fn new() -> Self;                // 全 0
    /// 取本帧槽位（不修改 fence 值；提交成功后才 `advance`）。
    pub(crate) fn acquire(&mut self, index: FrameIndex) -> FrameSlot;
    /// 提交成功后推进（`slot_values[slot] = signal_value`）。
    pub(crate) fn advance(&mut self, slot: FrameSlot);
    pub(crate) fn last_signaled(&self) -> u64;
}
```

### 4.3 `render/retire.rs` — 延迟销毁

```rust
/// 后端定义的资源句柄（P08 只有 `kind = Object`）。
///
/// 编码（**定死**）：`kind:u4 | generation:u28 | index:u32`，高位到低位。
/// - `kind`：1 = `GpuObject`（`ID3D12Resource`），2 = descriptor 槽位（P09），3 = upload arena（P09）；
///   0 与 4..15 未定义（`ResourceTable::release` 视为编程错误 → `debug_assert`）。
/// - `generation`：槽位复用代数（`ResourceTable` 每次分配 +1；陈旧句柄释放返回 `false`）。
/// - `index`：`ResourceTable` 的槽位下标。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct GpuResourceHandle(u64);

impl GpuResourceHandle {
    pub(crate) const fn new(kind: u8, generation: u32, index: u32) -> Self;
    pub(crate) const fn kind(self) -> u8;
    pub(crate) const fn generation(self) -> u32;
    pub(crate) const fn index(self) -> u32;
}

/// 主设计 §2.7 的共享状态机（与平台无关）。
pub(crate) struct RetireQueue {
    /// `(retire_at, handles)`；`retire_at` = 最后一次**可能**使用该资源的 `FrameIndex`。
    frames: VecDeque<(FrameIndex, Vec<GpuResourceHandle>)>,
    completed: FrameIndex,
}

impl RetireQueue {
    pub(crate) fn new() -> Self;
    /// 在 `FrameIndex` 的帧末仍在使用 → 到 `frame` 完成才可释放。
    pub(crate) fn retire(&mut self, at: FrameIndex, handle: GpuResourceHandle);
    /// fence 到达 `completed` 后调用：把 `retire_at <= completed` 的句柄交给 `free`（顺序稳定：先入先出）。
    pub(crate) fn on_completed(
        &mut self,
        completed: FrameIndex,
        free: &mut impl FnMut(GpuResourceHandle),
    );
    pub(crate) fn pending(&self) -> usize;
}
```

- retire 的语义是「**保守**」：本帧用过的资源至少活到本帧 fence 完成（同步提交下等价于「立即安全」，但结构不依赖这一点）。

### 4.4 `render/compositor.rs` — damage → Plan（纯函数，唯一翻译层）

```rust
/// 本帧的 present / copy 计划。**不分配**（固定 16 项，与 `MAX_DAMAGE_RECTS` 同源）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DamageRects {
    rects: [PhysicalRect; MAX_DAMAGE_RECTS],
    len: usize,
}

impl DamageRects {
    pub(crate) const fn new() -> Self;
    pub(crate) fn push(&mut self, rect: PhysicalRect) -> bool;   // false = 已满
    pub(crate) fn as_slice(&self) -> &[PhysicalRect];
    pub(crate) fn len(&self) -> usize;
    pub(crate) fn is_empty(&self) -> bool;
}

/// 一帧要提交的像素工作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    /// 没有任何像素要动（`commit_frame` 的第二段门禁已经拦掉，这里只作防御）。
    Idle,
    /// 整帧：全量 copy + 全量 present（`DirtyRectsCount = 0`）。
    Full,
    /// 局部：逐 `rects` copy + scissor + `Present1(dirty)`。
    Partial(DamageRects),
}

/// 三条输入：本帧 damage 摘要、合并后的物理矩形、surface 的物理矩形。
///
/// 规则（逐字）：
/// 1. `swapchain_fresh`（新建/`ResizeBuffers` 后/`damage.full`）→ `Full`；
/// 2. 逐个与 `surface` 求交，**丢弃空矩形**；任何越界都被夹紧（probe：越界 dirty rect → INVALID_CALL）；
/// 3. 夹紧后为空 → `Idle`；
/// 4. 数量 > `MAX_DAMAGE_RECTS` → `Full`（与 `DamageAccumulator` 的阈值同源，防御第二真值）。
pub(crate) fn plan(
    damage: DamageSummary,
    merged: &[PhysicalRect],
    surface: PhysicalRect,
    swapchain_fresh: bool,
) -> Plan;
```

### 4.5 `render/timer.rs` — 分段时间

```rust
/// P08 有生产者的段（`Build/Layout/Shape/Paint` 属 P09 的 App 级计时，additive 追加）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Section { Upload, Gpu, Present }

/// 一段的采样（CPU 与 GPU 至少一个非 0；`gpu_ns` 只在 `Section::Gpu` 有值）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GpuTimerSample { pub(crate) section: Section, pub(crate) cpu_ns: u64, pub(crate) gpu_ns: u64 }

/// 一帧的采样集合（容量复用；`FrameIndex` 对齐主设计 §2.10）。
pub(crate) struct FrameTimings { frame: FrameIndex, samples: Vec<GpuTimerSample> }
impl FrameTimings {
    pub(crate) fn new() -> Self;
    pub(crate) fn reset(&mut self, frame: FrameIndex);
    pub(crate) fn push(&mut self, sample: GpuTimerSample);
    pub(crate) fn samples(&self) -> &[GpuTimerSample];
}
```

---

## 5. Windows 后端规格

> 本节所有条目都已在 probe 里跑通（除 §2.7 标明的 resize 限制）。每个 `unsafe` 调用块都要 `// SAFETY:` 注释说明指针/生命期。

### 5.1 `windows/device.rs`

```rust
pub(crate) struct Device {
    raw: ID3D12Device,
    queue: ID3D12CommandQueue,
    caps: GpuCaps,
    resources: ResourceTable,
    log_path: PathBuf,          // DRED 落盘目录（= std::env::temp_dir()，无新依赖）
}

impl Device {
    /// 产品路径：DXGI 适配器 → D3D12 设备 → 采 caps → `caps.check()?`。
    /// DRED 设置必须在 **D3D12CreateDevice 之前**（probe §2.1）。
    pub(crate) fn new() -> Result<Self, EngineError>;

    /// test/CI 路径（WARP）。**`#[cfg(any(test, feature = "test-support"))]`**：
    /// 发布二进制不含软件设备（D8 / I10）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn new_warp() -> Result<Self, EngineError>;

    pub(crate) fn removed_reason(&self) -> Result<(), EngineError>;
    pub(crate) fn retire(&mut self, handle: GpuResourceHandle);
    pub(crate) fn drain_retire(&mut self, completed: FrameIndex);
}
```

- **适配器选择**：`IDXGIFactory4::EnumAdapters1(0)` 的第一个非软件适配器（probe：本机两份同 LUID 的 RTX 5080，取第一份即可；
  不按 VRAM 排序，避免与 P09 的 `IDXGIAdapter4` 选择逻辑形成两个真值来源）。`EnumWarpAdapter` 仅 test 路径。
- **D8 采集顺序**（与 §2.2 表一一对应）：`FEATURE_LEVELS` → `SHADER_MODEL`（入参 6_8，F-73）→ `D3D12_OPTIONS` →
  `OPTIONS4` → `ROOT_SIGNATURE` → `OPTIONS19`。`CheckFeatureSupport` 失败一律**不致命**（只把该字段填默认值并继续）——
  这些是记录项；硬基线只看三项。
- **`ResourceTable`**：
  ```rust
  pub(crate) struct ResourceTable {
      slots: Vec<Option<ID3D12Resource>>,
      generations: Vec<u32>,
      free: Vec<u32>,
      live: usize,
  }
  impl ResourceTable {
      pub(crate) fn insert(&mut self, resource: ID3D12Resource) -> GpuResourceHandle;
      /// 陈旧句柄（generation 不匹配）返回 `false` 并且**不**动槽位（类型系统外的 stale 引用在这里被拒绝）。
      pub(crate) fn release(&mut self, handle: GpuResourceHandle) -> bool;
      pub(crate) fn live(&self) -> usize;
  }
  ```
- **device lost**：`removed_reason()` 调 `GetDeviceRemovedReason()`；`Err` → 组装 `EngineError::DeviceLost { reason, log_path }`，
  其中 `reason = SmolStr::from(format!("{:#010X} {}", hr.0 as u32, hr.message()))`，`log_path` 是 `log_path.dred_log()` 生成的
  `kytrix-dred-<pid>-<frame>.txt`（`File::create` + 写 `GetAutoBreadcrumbsOutput` 的 HRESULT 与节点数；写失败只降级为路径本身）。

### 5.2 `windows/queue.rs`

```rust
pub(crate) struct Queue {
    raw: ID3D12CommandQueue,
    allocators: [ID3D12CommandAllocator; MAX_FRAMES_IN_FLIGHT],
    lists: [ID3D12GraphicsCommandList; MAX_FRAMES_IN_FLIGHT],
    fence: ID3D12Fence,
    event: HANDLE,
    ring: InFlightRing,
    completed: FrameIndex,
}

impl Queue {
    /// 等 slot fence → `allocator.Reset` → `list.Reset` → drain retire（`completed`）。
    pub(crate) fn begin(&mut self, device: &mut Device, index: FrameIndex)
        -> Result<FrameSlot, EngineError>;
    /// `Close` → `ExecuteCommandLists` → `Signal` → `SetEventOnCompletion` + `WaitForSingleObject`（同步）。
    /// 任一调用失败 → 先 `removed_reason()`（区分 device lost 与非法调用），再返回 `EngineError`。
    pub(crate) fn submit(&mut self, slot: FrameSlot) -> Result<(), EngineError>;
    pub(crate) fn wait_idle(&mut self) -> Result<(), EngineError>;   // resize / 快照 / 退出
    pub(crate) fn timestamp_frequency(&self) -> Result<u64, EngineError>;
}
```

- 事件对象：`CreateEventW(None, false, false, None)`（**需要 `Win32_Security`**，F-79）；`WaitForSingleObject(event, INFINITE)`。
- `submit` 后**立即**等待（同步提交）→ `completed = index` → `device.drain_retire(index)`。
  这保证「所有 GPU 写入都先确认对应 `FrameIndex` 的命令已提交」（主设计 §2.7 最后一条）。

### 5.3 `windows/swapchain.rs`

```rust
pub(crate) struct Swapchain {
    raw: IDXGISwapChain1,
    size: (u32, u32),
    format: DXGI_FORMAT,      // B8G8R8A8_UNORM（sRGB flip swapchain 被拒，见 §2.3）
    fresh: bool,              // 创建后 / ResizeBuffers 后 = true ⇒ 第一次 present 必须全量
}

impl Swapchain {
    pub(crate) fn for_hwnd(queue: &ID3D12CommandQueue, hwnd: HWND, size: (u32, u32))
        -> Result<Self, EngineError>;                 // P09 调用
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn for_composition(queue: &ID3D12CommandQueue, size: (u32, u32))
        -> Result<Self, EngineError>;                 // 无 HWND（probe §2.6 已验证 Present1 可用）
    /// `plan` 由调用方给（`Full` ⇒ `DirtyRectsCount = 0`）。
    pub(crate) fn present(&mut self, plan: Plan, sync: u32) -> Result<(), EngineError>;
    pub(crate) fn resize(&mut self, size: (u32, u32)) -> Result<(), EngineError>;
    pub(crate) fn back_buffer(&self) -> Result<ID3D12Resource, EngineError>;
}
```

**创建参数（定死）**：`{ Width/Height, Format: DXGI_FORMAT_B8G8R8A8_UNORM, Stereo: false, SampleDesc {1,0},
BufferUsage: RENDER_TARGET_OUTPUT, BufferCount: 3, Scaling: STRETCH, SwapEffect: FLIP_SEQUENTIAL,
AlphaMode: IGNORE, Flags: 0 }`。创建后 `GetDesc1()` 复核 `SwapEffect == FLIP_SEQUENTIAL && BufferCount >= 2`，否则
`EngineError::UnsupportedGpu { required: "…FLIP_SEQUENTIAL / BufferCount >= 2", actual }`。

**为什么 swapchain 用 `*_UNORM` 而不是 `*_UNORM_SRGB`**：①probe 实测 **sRGB flip swapchain 创建即失败**
（`0x887A0001`，§2.3），所以 `*_SRGB` 不是可选项；②`CopyTextureRegion` 要求源/目标**同格式**，而 persist 的资源格式必须是
UNORM 才能与 swapchain 匹配；③编码只发生一次——在 persist 的 **sRGB RTV 视图**上（`B8G8R8A8_UNORM` 资源 +
`B8G8R8A8_UNORM_SRGB` 视图，probe §2.3 实测 `clear(0.5) → 188`）。PSO 的 `RTVFormats[0] = B8G8R8A8_UNORM_SRGB`
必须与**视图**格式一致（不是资源格式）。
⇒ golden/快照读回的对象是 **persist**（UNORM 资源里的 sRGB 编码字节），与 present 路径解耦；读回的 footprint 描述用**资源格式**。

**`present` 的实现**（逐字）：

```text
let rects: [RECT; 16];  let n = match plan { Full => 0, Partial(r) => r.len() };
if self.fresh { n = 0; self.fresh = false; }            // F-72 ①：首帧/重建后必须全量
let params = DXGI_PRESENT_PARAMETERS { DirtyRectsCount: n,
    pDirtyRects: if n == 0 { null_mut() } else { rects.as_mut_ptr() },
    pScrollRect: null_mut(), pScrollOffset: null_mut() };  // 滚动提示 P12 再做（F-61 的 ScrollHint 延后）
let hr = raw.Present1(sync, DXGI_PRESENT(0), &params);     // 永不为 NULL（F-72 ③）
hr.ok()?;                                                  // 失败 → removed_reason() 分类
```

- **滚动**：P08 **不**发 scroll rect（`ScrollHint` 仍是 P12 的产物，F-61）；只留 `present` 的 `params` 位置。
- **`resize`**：`queue.wait_idle()` → drop 所有 back buffer 引用（P08 不建 RTV ⇒ 只有 `GetBuffer` 的临时引用）→
  `list.Reset`（在 `Queue::wait_idle` 内）→ `ResizeBuffers(0, 0, 0, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))` →
  `fresh = true`。失败 → `EngineError::Swapchain { detail }`，**不重试**（§2.7）。

### 5.4 `windows/pipeline.rs`

```rust
pub(crate) struct Pipeline {
    root_signature: ID3D12RootSignature,     // 184 B blob，probe §2.4
    quad: ID3D12PipelineState,
    geometry: ID3D12PipelineState,           // 顶点布局见下
    srv_heap: ID3D12DescriptorHeap,          // 静态分区，见下
    shaders: ShaderSet,                      // include_bytes! + magic 校验（P07 S-71）
}

pub(crate) struct ShaderSet { quad_vs: &'static [u8], quad_ps: &'static [u8],
                              geometry_vs_ui: &'static [u8], geometry_vs_textured: &'static [u8],
                              geometry_ps_ui: &'static [u8], geometry_ps_image: &'static [u8],
                              geometry_ps_glyph: &'static [u8] }
```

| 项 | 定死值 |
|---|---|
| 根签名 | 参数 0：`32BIT_CONSTANTS`，`ShaderRegister = 0`，`Num32BitValues = 28`，`VISIBILITY_ALL`；参数 1：`DESCRIPTOR_TABLE`（1 range：SRV `t0..t3`，4 个描述符），`VISIBILITY_ALL`；静态采样器 `s0`（`MIN_MAG_MIP_LINEAR` / `CLAMP×3` / `COMPARISON_NEVER` / `TRANSPARENT_BLACK` / `MinLOD 0` / `MaxLOD 0` / `PIXEL`）；`Flags = ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT` |
| SRV 堆 | `CBV_SRV_UAV`，`NumDescriptors = 3 * 4 + 2 = 14`，`SHADER_VISIBLE`；每槽布局 `[instances, clips, glyph, image]`（槽 0..2 各 4 个），slot 12/13 = 预留（快照/图集重建时的临时 SRV） |
| quad PSO | 无输入布局；`PrimitiveTopologyType = TRIANGLE`；`IBStripCutValue = DISABLED`；`RasterizerState = { FILL_SOLID, CULL_NONE, FrontCCW false, DepthClip true, Multisample false, ForcedSampleCount 0, ConservativeRaster OFF }`；`DepthStencil = { DepthEnable false, StencilEnable false, DepthFunc ALWAYS }`；`BlendState`：RT0 `{ BlendEnable, SrcBlend ONE, DestBlend INV_SRC_ALPHA, BlendOp ADD, SrcBlendAlpha ONE, DestBlendAlpha INV_SRC_ALPHA, BlendOpAlpha ADD, LogicOp NOOP, WriteMask 0x0F }`；`SampleMask = u32::MAX`；`NumRenderTargets = 1`；`RTVFormats[0] = B8G8R8A8_UNORM_SRGB`；`DSVFormat = UNKNOWN`；`SampleDesc {1,0}`。**拓扑在录制时**用 `IASetPrimitiveTopology(TRIANGLESTRIP)`（probe 的负例证明 `TRIANGLELIST` 只出一个三角形） |
| geometry PSO | 输入布局两条：`POSITION`（`R32G32_FLOAT`，offset 0）与 `TEXCOORD0`（`R32G32_FLOAT`，offset 8）；两个 PSO 共享 `VS` 的 stride 区分（`Ui` stride 8 / `Textured` stride 16，`InputLayout` 各一份）；`PrimitiveTopologyType = TRIANGLE`；`IASetPrimitiveTopology` 按 `Topology::{TriangleList, TriangleStrip}` 在录制时设置；其余状态与 quad 相同 |
| 混合 | 与 quad 完全一致（同一个 `D3D12_BLEND_DESC` 常量，避免两个真值来源） |

**`GeometryRegistry`（P08 的 `pub(crate)` 最小实现，P09 的 `GeometryStore` 接在它上面）**：

```rust
pub(crate) struct GeometryRegistry {
    entries: Vec<Option<GeometryEntry>>,   // 下标 = GeometryId::index()
}
struct GeometryEntry {
    vertices: GpuResourceHandle,           // 句柄（Drop 走 retire，不直接持 COM）
    indices: GpuResourceHandle,
    vertex_buffer: D3D12_VERTEX_BUFFER_VIEW,
    index_buffer: D3D12_INDEX_BUFFER_VIEW,
    index_count: u32,
    topology: D3D_PRIMITIVE_TOPOLOGY,
    pipeline: GeometryPipeline,
}

impl GeometryRegistry {
    /// `#[cfg(any(test, feature = "test-support"))]`：P08 没有公开注册 API（F-65 要求 P09 才删除 `for_test`）。
    pub(crate) fn register_for_test(&mut self, id: GeometryId, desc: &GeometryDesc) -> Result<(), EngineError>;
    pub(crate) fn lookup(&self, id: GeometryId) -> Option<&GeometryEntry>;
    pub(crate) fn clear(&mut self);        // 全部 retire
}
```

`FrameOp::Geometry` 的 `lookup` 失败（P08 的生产构建必然如此，因为没有注册入口）→ `EngineError::GeometryRegister { detail }`
（detail = `format!("geometry id {}:{} has no GPU buffers (GeometryStore lands in P09)", index, generation)`）。
**不做静默跳过**（I2）。

### 5.5 `windows/atlas.rs`

```rust
pub(crate) struct AtlasTextures {
    glyph: AtlasTexture,     // R8_UNORM，2048×2048
    image: AtlasTexture,     // B8G8R8A8_UNORM_SRGB，2048×2048
}
struct AtlasTexture {
    resource: ID3D12Resource,
    upload: ID3D12Resource,  // 每次上传复用的 staging（256 B 行对齐；容量 = 页宽 × 8 行 × 纹素字节）
    generation: u32,         // 与 AtlasAllocator::generation 对齐
}
```

- **上传粒度**：`DirtyRows::spans()`（P05 已落地的行区间）→ 每段一次 `CopyTextureRegion`（SRC = staging 的 placed footprint，
  `SRC_BOX = {0, y0, 0, width, y0 + rows, 1}`，`DST = (0, y0)`）。`is_full()` → 整页一次。
- staging 每次上传前 `Map` 写、`Unmap`；行 pitch 取 `align_up(width * texel_bytes, 256)`。
- **generation 变化**（`AtlasAllocator::clear()` 或页尺寸变化）→ 旧纹理 + staging `retire`，新建纹理，**本帧全量重传**
  （P05 §10.6：`clear` 后引用方全部 `PAINT`，像素会重新产生）。
- SRV：`R8_UNORM` 与 `B8G8R8A8_UNORM_SRGB`（glyph **不**用 sRGB，P07 §2.9），`ResourceMinLODClamp 0`、`MipLevels 1`、`PlaneSlice 0`。
- **像素来源**：P08 **不**持有 CPU 像素副本（P05 §10.6 的定案）；上传接口收 `&[u8] + 行区间 + 页宽`，由 P06/P15 调用
  （P08 的图形测试用 test-only 的填充函数）。

### 5.6 `windows/compositor.rs` — `Renderer`（帧编排，唯一 unsafe 汇聚点）

```rust
pub(crate) struct Renderer {
    device: Device,
    queue: Queue,
    pipeline: Pipeline,
    swapchain: Option<Swapchain>,     // None = 纯离屏（快照/测试）
    persist: Persist,                 // 持久 offscreen（§6.3）
    atlas: AtlasTextures,
    geometry: GeometryRegistry,
    upload: UploadArena,              // 每 in-flight 槽的实例/裁剪缓冲
    timer: Option<GpuTimer>,
    clear_color: [f32; 4],            // 默认 [0.0, 0.0, 0.0, 0.0]（透明；P09 可设为主题背景）
    damage_rects: [RECT; MAX_DAMAGE_RECTS],
    surface: (u32, u32),
    fresh_present: bool,
}

impl Renderer {
    pub(crate) fn new_offscreen(size: (u32, u32)) -> Result<Self, EngineError>;  // 快照/测试
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn new_warp_offscreen(size: (u32, u32)) -> Result<Self, EngineError>;
    #[cfg(windows)]
    pub(crate) fn new_for_hwnd(hwnd: HWND, size: (u32, u32)) -> Result<Self, EngineError>;  // P09

    /// 一帧的全部 GPU 工作。`plan` 由共享层 `compositor::plan` 产出。
    pub(crate) fn submit(
        &mut self,
        index: FrameIndex,
        buffer: &InstanceBuffer,
        plan: Plan,
        timings: &mut FrameTimings,
    ) -> Result<(), EngineError>;

    /// 读回持久 offscreen（快照；阻塞到 GPU 完成）。
    pub(crate) fn readback(&mut self) -> Result<SnapshotImage, EngineError>;

    pub(crate) fn set_clear_color(&mut self, color: [f32; 4]);
    pub(crate) fn surface(&self) -> (u32, u32);
}
```

### 5.7 `windows/readback.rs`

```rust
pub(crate) struct Readback { staging: IdleBuffer }   // 容量按 surface 的 `row_pitch * height` 复用
impl Readback {
    /// persist → COPY_SOURCE → `CopyTextureRegion` → READBACK buffer → `Map` → BGRA→RGBA + 去 padding。
    pub(crate) fn texture(
        &mut self,
        device: &Device,
        queue: &mut Queue,
        texture: &ID3D12Resource,
        size: (u32, u32),
        format: DXGI_FORMAT,
    ) -> Result<SnapshotImage, EngineError>;
}
```

### 5.8 `windows/timer.rs`

```rust
pub(crate) struct GpuTimer {
    heap: ID3D12QueryHeap,             // TIMESTAMP × 2 × MAX_FRAMES_IN_FLIGHT
    resolve: [GpuResourceHandle; 3],   // READBACK buffer（每槽 16 B）
    frequency: u64,                     // GetTimestampFrequency（probe：1e9）
}
impl GpuTimer {
    pub(crate) fn new(device: &Device, queue: &Queue) -> Result<Self, EngineError>;
    pub(crate) fn begin(&self, list: &ID3D12GraphicsCommandList, slot: usize);
    pub(crate) fn end(&self, list: &ID3D12GraphicsCommandList, slot: usize, resolve_target: &ID3D12Resource);
    /// fence 到达后读两个 u64，返回 `Section::Gpu` 样本（tick → ns 用 `frequency`）。
    pub(crate) fn resolve(&self, slot: usize) -> Option<GpuTimerSample>;
}
```

`GpuTimer::new` 失败（WARP 上 `GetTimestampFrequency` 可能失败）→ `timer: None`，`FrameTimings` 里没有 `Section::Gpu`
（**不是**错误：计时是可选能力，不影响像素）。

---

## 6. 帧流程（逐字）

### 6.1 `Window::commit_frame` 的提交点

```text
commit_frame(ticket, report):
  has_pixels = report.rebuilt > 0 || report.damage.full || report.damage.rects > 0
  if !has_pixels { abort(ticket); return Ok(Idle) }                  // 既有第二段门禁
  let index = scheduler.commit(ticket)                               // FrameIndex 唯一递增点
  tree.settle_frame()                                               // 既有（F-59）
  #[cfg(windows)] if let Some(renderer) = self.renderer.as_mut() {
      let surface = physical surface（size × scale，covering）
      let fresh = renderer.surface() != surface || renderer.is_fresh()
      if fresh { renderer.resize(surface)? }                         // 见 §6.2
      let plan = compositor::plan(report.damage, self.damage.rects(), surface_rect, fresh)
      if plan != Plan::Idle {
          renderer.submit(index, self.instance_buffer(), plan, &mut timings)?   // Err ⇒ 本帧像素没出去
      }
  }
  #[cfg(feature = "test-support")] { record capture }
  Ok(Committed(index))
```

- **错误语义**：`renderer.submit` 的 `Err` 直接返回给 `App::frame`（`Some(Err(e))`）；`FrameIndex` **已经**递增（帧确实提交过），
  dirty/damage 已在 `settle_frame` 结算 —— 因此**不重试本帧**，错误向上冒泡（device lost 时后续帧会持续返回同一错误，见 §8）。
- headless（`renderer == None`）路径**逐字节不变**。

### 6.2 `Renderer::submit` 逐步（含 barrier 表）

```text
submit(index, buffer, plan, timings):
  1. let slot = queue.begin(device, index)?                       // 等 fence[slot] → 复位 allocator/list → drain retire
  2. surface 变化（resize）：
       queue.wait_idle()?                                          // 已同步，但显式一次
       swapchain.resize(surface)?                                  // fresh = true
       persist.recreate(surface)?                                  // 旧纹理 retire
  3. 上传（t0/t1）
       upload.write_instances(slot, buffer.as_bytes())             // 只写 [0, len*128)；容量不够 → 扩容（旧缓冲 retire）
       buffer.write_clips_gpu(&mut clips_scratch)                    // 48 B/层，表项 0 是哨兵（容量复用）
       upload.write_clips(slot, &clips_scratch)
       pipeline.write_srv(slot, &upload, &atlas)                   // 每槽 4 个 SRV（instances/clips/glyph/image）
  4. persist：RT → RT（本帧第一件事）
       if plan == Full { ClearRenderTargetView(persist, clear_color) }   // 全帧必须清（damage 外没有「上一帧」可依赖）
       RSSetScissorRects(每个 damage 矩形；Full ⇒ 整面)
       对每个 damage 矩形：逐 FrameOp 画（instances → DrawInstanced(4, len, 0, start)；geometry → DrawIndexedInstanced）
       persist: RENDER_TARGET → COPY_SOURCE
  5. back buffer：PRESENT → COPY_DEST
       对每个 damage 矩形：CopyTextureRegion(dst=back@(x,y), src=persist, SRC_BOX=矩形)
       （Full ⇒ 一次整面 copy）
       back: COPY_DEST → PRESENT
  6. timer：EndQuery(slot, 1) + ResolveQueryData（§5.8；放在 copy 之后）
  7. queue.submit(slot)?                                          // Close → Execute → Signal → 等待
  8. swapchain.present(plan, sync = 1)?                            // 有 swapchain 时；失败 → removed_reason() 分类
  9. device.drain_retire(index)（由 queue.submit 内部完成）
 10. timings：Upload（CPU）、Gpu（GPU）、Present（CPU）三段
```

**barrier 表（每帧固定，无动态状态机）**：

| 资源 | before | after | 时机 |
|------|--------|-------|------|
| persist | `RENDER_TARGET` | `COPY_SOURCE` | draw 之后、copy 之前 |
| persist | `COPY_SOURCE` | `RENDER_TARGET` | copy 之后（下一帧的第一件事就是画它） |
| back buffer | `PRESENT` | `COPY_DEST` | copy 之前 |
| back buffer | `COPY_DEST` | `PRESENT` | copy 之后、`Present1` 之前 |
| upload buffers | `GENERIC_READ` | `GENERIC_READ` | 无（UPLOAD 堆，CPU 写后 GPU 读，fence 保证不重叠） |
| atlas 纹理 | `PIXEL_SHADER_RESOURCE` | `COPY_DEST` → `PIXEL_SHADER_RESOURCE` | 仅当有脏行 |

### 6.3 持久 offscreen（格式 / 内容权威 / 与 swapchain 的关系）

| 项 | 定死值 |
|---|---|
| 资源格式 | `DXGI_FORMAT_B8G8R8A8_UNORM`；**RTV 视图格式 = `DXGI_FORMAT_B8G8R8A8_UNORM_SRGB`**（P07 §2.3 的唯一编码点；probe §2.3 实测 clear(0.5)→188） |
| 采样 | `SampleDesc { Count: 1, Quality: 0 }`、`MipLevels 1`、`ArraySize 1`、`Flags = ALLOW_RENDER_TARGET` |
| 尺寸 | = surface 的物理像素（`size × scale` 的 covering 结果）；resize 时重建 |
| 内容权威 | **是**：damage 外的像素永远由它保持；back buffer 只是它的「本帧脏矩形切片」 |
| 全帧帧 | 先 `ClearRenderTargetView(clear_color)` 再画（不清会留下上一帧的残影；damage 外的像素在这一帧本来就会被重画或清掉） |
| 局部帧 | 不 clear；scissor 内先画覆盖上一帧内容（premultiplied 混合需要正确底子 → 因为 persist 是完整的，直接画即正确） |
| 与 swapchain 的 copy | `CopyTextureRegion`（**同格式 `B8G8R8A8_UNORM`**、1:1、无缩放），`SRC_BOX = damage`，`DST = (damage.left, damage.top)`；`Full` 时用 `CopyResource`（整面，probe frame1 已验证） |
| N-buffer 陈旧 | **不需要**「拷回前 N−1 帧」：本帧的每个 damage 矩形在 present 前都被写成了正确内容（copy + 可能的重画），而 present 只声明这些矩形。`fresh`/`Full` 帧会把整面写一遍 |

### 6.4 `Present1` 四条硬约束 → 落地检查单

| # | 约束（主设计 §2.4） | P08 的落地方式 | 验证 |
|---|---|---|---|
| 1 | 必须更新所报告矩形内的每一个像素 | 每个 damage 矩形都先从 persist `CopyTextureRegion`（未变化像素）+ 再画（变化像素）；`Full` 时整面 `CopyResource` | probe §2.5（frame2 的 `(30,30)` 与 `(140,140)` 同时正确） |
| 2 | 不得更新脏矩形之外的任何像素 | 绘制用 `RSSetScissorRects(damage)`；copy 用 `D3D12_BOX = damage`；**没有**对 back buffer 的 RTV/clear | probe §2.5（damage 外像素保持）；单测 `compositor::plan` 的夹紧 |
| 3 | flip model + BufferCount ≥ 2；scroll/dirty 仅 flip；MSAA swapchain 不支持 dirty/scroll | `FLIP_SEQUENTIAL` + `BufferCount 3`；`GetDesc1` 复核；MSAA swapchain 创建即失败（probe §2.6） | probe §2.6 |
| 4 | N-buffer 的当前 back buffer 陈旧 | §6.3 的结构（persist 为权威 + 每帧按 damage 写 back） | probe §2.5 / §6.3 表 |

**追加检查单（本机 probe 新增，F-72）**：

| # | 规则 | 违反后果 |
|---|---|---|
| 5 | 创建后 / `ResizeBuffers` 后**第一次 present 必须全量**（`DirtyRectsCount = 0`） | `0x887A0001` |
| 6 | dirty rect 必须**非空且完全落在 surface 内** | `0x887A0001` |
| 7 | `pPresentParameters` **不得为 NULL** | 进程终止（AV） |
| 8 | `pScrollOffset` 必须与 `pScrollRect` 成对（DXGI 不校验） | 视觉错位（无错误码） |

---

## 7. 资源生命周期 / 上传 / readback

### 7.1 实例缓冲（只写不重建容量）

| 项 | 定死值 |
|---|---|
| 形态 | **每 in-flight 槽一块 UPLOAD buffer**（3 块），`Width = capacity * 128`，初始 `capacity = 256`（32 KB） |
| 基址对齐 | UPLOAD 堆资源天然 64 KB 对齐（≥ F-23 的 16 B） |
| 扩容 | 本帧 `len > capacity` → 新缓冲 = `next_power_of_two(len)`（一次性）+ 旧句柄 `retire`；**不**按帧翻倍 |
| 每帧写入 | 只写 `[0, len*128)`（`Map` → `copy_nonoverlapping` → `Unmap`）；SRV `NumElements = len` |
| resize | **不重建**（容量与 surface 无关）—— 主设计 §2.9 的关键 |

### 7.2 裁剪表

每槽一块 UPLOAD buffer（`capacity * 48`，初始 8 层）；`ClipChain → ClipEntry` 的转换在 `paint/instance.rs` 里做（P08 新增）：

```rust
/// GPU 侧 `ClipEntry`（P07 §2.6）：48 B / 16 B 对齐，**丢弃 aabb**，parent 进 uint4.x。
#[repr(C, align(16))]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct GpuClipEntry { rect: [f32; 4], corner_radius: [f32; 4], meta: [u32; 4] }
const _: () = { assert!(size_of::<GpuClipEntry>() == 48); assert!(align_of::<GpuClipEntry>() == 16); };

impl ClipChain {
    /// 表项 0 的哨兵语义由 `clip_index == 0` 的早退保证（P07 §2.6 的哨兵陷阱），这里照抄 CPU 值。
    pub(crate) fn gpu_entry(&self) -> GpuClipEntry;
}
impl InstanceBuffer {
    pub(crate) fn slots(&self) -> ...;               // P08 需要的最小只读面
    /// 把 `clips()` 转成 GPU 布局写进调用方预分配的 `Vec`（容量复用，不每帧分配）。
    pub(crate) fn write_clips_gpu(&self, out: &mut Vec<GpuClipEntry>);
}
```

### 7.3 `InstanceBuffer` 的上传访问器（P06 遗留 ④ 的收口）

```rust
impl InstanceBuffer {
    /// 实例字节（已有，非 test）。
    pub(crate) fn as_bytes(&self) -> &[u8];
    /// 逐 `FrameOp` 的绘制序（P08 的 `queue`/`compositor` 唯一读者）。
    pub(crate) fn ops(&self) -> &[FrameOp];
    /// `FrameOp::Geometry.index` 的下标表。
    pub(crate) fn geometry(&self) -> &[GeometryDraw];
    /// 裁剪表（含表项 0 哨兵）。
    pub(crate) fn clips(&self) -> &[ClipChain];
}
```

### 7.4 描述符 / 堆

| 堆 | 容量 | 生命周期 |
|---|---|---|
| `CBV_SRV_UAV`（shader visible） | 14 | 与 `Renderer` 同生共死；每槽 4 个 SRV 在 `begin` 后重写（`CreateShaderResourceView` 覆盖同一 `ptr`） |
| `RTV` | 2（persist + 快照目标） | persist 重建时重写；**swapchain buffer 永不建 RTV** |
| 采样器 | 0（静态在根签名里） | — |

**描述符回收**：P08 静态分区 ⇒ **没有**需要回收的索引（S-75）。P09 引入动态 SRV 时用同一个 `GpuResourceHandle` 的
`kind = 2` 编码 + `DescriptorSlab`（free list + generation），`RetireQueue` 的接口不变。

### 7.5 device lost 路径（停止提交 → DRED 落盘 → 可读错误）

```text
任一 D3D12 调用返回失败：
  → device.removed_reason()
      Ok(())  ⇒ 参数/状态错误（保留原 HRESULT，按 EngineError::Swapchain / ShaderLoad / AtlasExhausted 分类）
      Err(lost) ⇒ 组装 EngineError::DeviceLost { reason, log_path }
                  renderer.lost = Some(lost.clone())        // 粘性：停止一切提交
                  return Err(lost)
后续帧：submit 入口 `if let Some(lost) = &self.lost { return Err(lost.clone()) }`   // 不再碰 D3D12
DRED：log_path 文件写 `GetAutoBreadcrumbsOutput()` 的 HRESULT + `pHeadAutoBreadcrumbNode` 非空时遍历
      （`D3D12_AUTO_BREADCRUMB_NODE` 最多 16 个节点，每节点写 `pCommandListDebugNameA` 与 `BreadcrumbCount`），
      写失败吞掉（错误对象已带路径）
```

- **不重建** device（主设计 §2.9 定案）。
- `DeviceLost.reason` 用 `format!("{:#010X} {}", hr.0 as u32, hr.message())`（probe：`0x887A0005` + 系统消息可读）。
- **device lost 的测试**：`ID3D12Device5::RemoveDevice`（probe 已验证）+ WARP 设备 → 断言 `Err(DeviceLost)` 且 `log_path` 存在。

### 7.6 上传顺序与 fence 的交点

`begin` 等到 `slot` 的 fence → 该槽的 upload buffer / allocator / list 全部可安全复用 → 写 → 提交 → signal。
**没有**任何跨槽共享的可写 GPU 资源（实例/裁剪/时间戳缓冲都是每槽独立）。

### 7.7 readback 与快照

```text
snapshot(case):
  1. 物理尺寸 = (case.size * case.scale).covering
  2. Renderer::new_warp_offscreen(size)（每次调用新建；快照是测试路径，设备创建 ~10 ms）
  3. 用 TestApp 当前的 `InstanceBuffer` 走「full damage」一帧（clear + draw，无 swapchain）
  4. readback：persist(RT) → COPY_SOURCE → CopyTextureRegion 到 READBACK buffer（footprint 用**资源格式** `B8G8R8A8_UNORM`，见 §2.3）
     → 等 fence → Map → 逐行去 padding → BGRA→RGBA
  5. SnapshotImage { width, height, rgba8 }（**预乘** sRGB 编码字节）
```

| 项 | 定死值 |
|---|---|
| 行 pitch | `GetCopyableFootprints` 的 `RowPitch`（≥ `width*4`，256 B 对齐；probe 256×256 → 1024） |
| alpha | 预乘（P07 §2.3）；golden 与实现同源，故 golden 里也是预乘 |
| 字节序 | 读回是 BGRA，`SnapshotImage` 是 RGBA（交换 R/B，A 不动） |
| 容差机制（P08 落，数值 P18 定） | `SnapshotDiff { max_channel_diff: u8, avg_channel_diff: f32, over_threshold_pixels: u32 }` + `fn compare(a, b, tolerance: SnapshotTolerance) -> SnapshotDiff`；`SnapshotTolerance { per_channel: u8, over_threshold_pixels: u32 }`，默认值用 P07 §7.4 的 `1`（并注明 P7 实测定值） |
| WARP vs 硬件 | 只比结构（尺寸/alpha 语义/字节序）与容差内像素；**不**在 P08 冻结两设备差值 |
| 非 Windows | `SnapshotError::NoRenderer`（I7 不变） |

---

## 8. `GpuResourceHandle` 与 retire 的完整闭环

| 场景 | 谁 retire | 何时释放 |
|---|---|---|
| 实例缓冲扩容（旧 UPLOAD buffer） | `UploadArena::grow` → `queue.retire(frame, handle)` | 该 `FrameIndex` 的 fence 完成（同帧内已有等待，故实际上立即） |
| resize（旧 persist + 旧 staging） | `Renderer::resize` → retire | 同上 |
| 图集重建（旧纹理 + 旧 staging） | `AtlasTextures::rebuild` → retire | 同上 |
| 快照（临时 persist/readback buffer） | `Readback`/`Renderer::drop` → 直接 drop（同步等待已完成） | 立即 |
| geometry 注销（P09） | `GeometryRegistry` → retire | P09 |

`RetireQueue::on_completed` 由 `Queue::submit` 在 fence 到达后调用；`ResourceTable::release` 的 `false`（陈旧句柄）→ `debug_assert`。

---

## 9. 与 `Window` / `App` / `test_support` 的接线

### 9.1 `Window`（新增字段与方法）

见 §3.3。`Window::instance_bytes()` 保持 `pub`（P06 公开面），内部改为 `self.instance_buffer().as_bytes()`。

### 9.2 `App`

`App::frame` 无需结构变化（`commit_frame` 的 `Result` 已把 GPU 错误上抛）。`App::create_window` 不创建 renderer
（P09 的窗口层做）——P08 的 `TestApp` 快照走独立离屏 `Renderer`。

### 9.3 `test_support`（`--features test-support`）

```rust
// test_support/snapshot.rs
pub enum SnapshotError {
    /// 本构建没有渲染器（非 Windows / 未编译 GPU 支持）。
    NoRenderer,
    /// GPU 路径失败（设备创建 / 渲染 / 读回）。
    Gpu { detail: SmolStr },
}
impl SnapshotImage { /* 字段不变 */ }

// test_support/app.rs
impl TestApp {
    pub fn snapshot(&self, case: SnapshotCase) -> Result<SnapshotImage, SnapshotError> {
        #[cfg(windows)]
        {
            // 每次新建 WARP 离屏渲染器（测试路径；设备创建 ~10 ms）
            let mut renderer =
                crate::render::backend::Renderer::new_warp_offscreen(physical(case))
                    .map_err(|e| SnapshotError::Gpu { detail: e.to_string().into() })?;
            let mut timings = FrameTimings::new();
            renderer
                .submit_snapshot(self.window().instance_buffer(), &mut timings)
                .map_err(|e| SnapshotError::Gpu { detail: e.to_string().into() })?;
            renderer
                .readback()
                .map_err(|e| SnapshotError::Gpu { detail: e.to_string().into() })
        }
        #[cfg(not(windows))]
        {
            let _ = case;
            Err(SnapshotError::NoRenderer)
        }
    }
}
```

`TestApp` 的其余 API 不变（`renderer = None` 的 headless 语义仍在 `App`/`Window` 上，`snapshot` 只是额外能力）。

---

## 10. 测试与门禁

### 10.1 单测清单（计划 **45** 项；Windows 专属项标 `#[cfg(windows)]`）

| 文件 | # | 用例 |
|---|---|---|
| `render/device.rs` | 6 | `baseline_ok` 真值表（FL/SM/Tier 各低于一次）；`actual()` 的三段格式；`check()` → `UnsupportedGpu{required,actual}`；`Display` 同时含 required/actual 且无降级措辞；`describe()` 含全部字段；`fl_name`/`sm_name` 映射表 |
| `render/frame.rs` | 4 | 槽位 0,1,2,0 循环；`wait_value` 首次全 0；`advance` 后单调；`FrameIndex(0)` 在 debug 触发断言 |
| `render/retire.rs` | 5 | 同帧 retire → 同帧完成即释放；未完成保留；跨帧顺序 FIFO；`pending` 计数；空队列 |
| `render/compositor.rs` | 7 | `full` 直通；空 + 非 full → `Idle`；矩形夹到 surface；越界全裁 → `Idle`；数量 > 16 → `Full`；`swapchain_fresh` 强制 `Full`；`DamageRects::push` 满时返回 false |
| `render/timer.rs` | 2 | `FrameTimings` 复用容量且 `reset` 清空；`Section` 顺序稳定 |
| `paint/instance.rs`（追加） | 2 | `GpuClipEntry` 的 48 B/16 B/字段 offset；`clips()` 含哨兵且 `parent` 正确 |
| `render/windows/device.rs` | 3 | WARP caps 满足基线；`ResourceTable` 分配-释放-复用后 generation 递增、旧句柄释放失败；`removed_reason` 在 `RemoveDevice` 后返回 `DeviceLost` |
| `render/windows/pipeline.rs` | 2 | 根签名 blob 可创建（WARP）；quad/geometry PSO 均可创建（WARP） |
| `render/windows/atlas.rs` | 2 | 上传 `[y0,y1)` 行后读回只有这些行变化；`generation` 变化后纹理重建且内容为全量重传 |
| `render/windows/compositor.rs` | 6 | WARP 离屏：`clear(0.5) == 188`（UNORM 资源 + SRGB 视图只编码一次）；quad 像素（同 probe：中心红、四角黑）；damage 二次帧（新色 + 持久像素）；**实例上传字节**（`Map` 读回前 `len*128` 与 `as_bytes()` 逐字节相等、SRV `NumElements == len`）；resize 后实例容量**不变**且像素正确；`Plan::Idle` 不产生任何 GPU 工作 |
| `render/windows/readback.rs` | 2 | 奇数宽度（如 37×11）的 row-pitch 处理；BGRA→RGBA 交换 |
| `render/windows/timer.rs` | 2 | WARP timestamp resolve 非递减（失败则 `timer=None` 路径不 panic）；tick→ns 换算 |
| `render/windows/swapchain.rs`（纯逻辑） | 2 | `fresh` 时 `DirtyRectsCount = 0`；dirty rect 裁剪/为空时降级全量 |

（合计 45 项；若实现期发现某项与既有 418 基线重叠可合并 —— 以「每条 `pub(crate)` 有非 test 消费者 + 覆盖 §6.4 的 8 条约束」为下限。）

### 10.2 门禁命令（Windows 本机）

```powershell
# 1) 单测（4 组：debug/release × 默认/test-support）
cargo test -p kytrix_gui
cargo test -p kytrix_gui --release
cargo test -p kytrix_gui --features test-support
cargo test -p kytrix_gui --release --features test-support

# 2) lint / 格式（含测试与 build.rs）
cargo +nightly clippy -p kytrix_gui --all-targets -- -D warnings
cargo +nightly clippy -p kytrix_gui --all-targets --all-features -- -D warnings
cargo +nightly fmt -p kytrix_gui -- --check

# 3) 三平台 check（非 Windows 不编译 render/windows；shader 走 skip 占位）
$env:KYTRIX_GUI_SKIP_SHADERS = "1"
cargo check -p kytrix_gui --target aarch64-apple-darwin
cargo metadata --no-deps --filter-platform x86_64-unknown-linux-gnu --offline   # rc=0 即可（已知 fontconfig 交叉限制）
Remove-Item Env:KYTRIX_GUI_SKIP_SHADERS

# 4) 模块根形态（render.rs / render/windows.rs 只允许 mod / pub(crate) mod / pub(crate) use）
Select-String -Path crates/kytrix_gui/src/render.rs,crates/kytrix_gui/src/render/windows.rs -Pattern '^(?!\s*//)(?!.*\b(mod|use)\b).+'
#   期望无输出

# 5) 依赖零变化
git diff --stat -- Cargo.toml Cargo.lock     # 只允许 Cargo.toml 的 windows features 加一行（F-79）
```

### 10.3 D8 失败路径（无法强制时注入 caps）

本机两设备都满足基线 ⇒ 用**纯单测**覆盖（不需要 GPU）：

```rust
let caps = GpuCaps { feature_level: 45056 /* 11_0 */, shader_model: 96 /* 6_0 */, resource_binding_tier: 1, ..warp_caps() };
let error = caps.check().unwrap_err();
assert!(matches!(error, EngineError::UnsupportedGpu { os: "windows", .. }));
assert!(error.to_string().contains("FL 11_0 / SM 6_0 / BindingTier 1"));
```

`swapchain` 的 FLIP_SEQUENTIAL/BufferCount 失败路径同理：`Swapchain::validate(&desc1)` 抽成纯函数，单测注入
`SwapEffect = DISCARD` 的 `DXGI_SWAP_CHAIN_DESC1` 断言 `UnsupportedGpu`。

### 10.4 交互式/实机验证（不进 CI，登记为 V-55）

- `KYTRIX_GUI_D3D12_HWND_TESTS=1` 时才跑：真 HWND（隐藏窗口）建 swapchain → `Present1(dirty)` → `ResizeBuffers`。
  本机非交互会话下 resize 段已知失败（§2.7），**默认 `#[ignore]`** 且不计入门禁；P09 在可见窗口会话上把它转正。

---

## 11. 风险

| # | 风险 | 影响 | 缓解 |
|---|---|---|---|
| R-1 | **描述符堆/根签名/PSO 细节漂移**（`Num32BitValues` 写错、`ShaderVisibility` 只给 PIXEL、`TriangleList` 笔误） | 黑屏/半个 quad/无法创建 PSO | probe 已逐条跑通并留代码骨架（§2.4）；单测覆盖「PSO 可创建」+「WARP 像素」；`contract.md` 与 `pipeline.rs` 的常量同源（P07 §2.9） |
| R-2 | **`windows 0.62.2` 的方法名/返回类型差异**（`Present1 -> HRESULT`、`ResizeBuffers -> Result`、`RemoveDevice` 在 `ID3D12Device5`、`CreateEventW` 被 `Win32_Security` 门控） | 编译期即失败 | §2.9 的逐条事实 + F-79；实现期以 §2.9 为清单 |
| R-3 | **WARP 能力集**（本机 WARP = FL 12_1 / SM 6_8 / Tier3，`max_sampler_heap = 2097152` 与硬件 4080 不同） | CI golden 与真机分叉 | 不启用 SM6.6 路径（S-74）；快照只比结构与容差内像素；golden 按设备分组（主设计 §6.4） |
| R-4 | **readback 同步与格式**（早 `Map` 读到垃圾、`RowPitch` 手算错、忘记 `Unmap`、footprint 用视图格式而非**资源格式**） | 快照随机花屏/UB/尺寸错 | `GetCopyableFootprints` 唯一来源且用资源格式（`B8G8R8A8_UNORM`）；每槽独立 READBACK buffer；probe §2.3 的数字写进单测（256×256 → 1024） |
| R-5 | **`ResizeBuffers` 在本机（非交互会话 + 隐藏窗口）失败**，且失败会污染 allocator | resize 路径无法在本机验证 | §2.7 的处置（纯逻辑单测 + 失败即致命 + P09 实机 V-55）；实现期**不得**把「失败重试」写成兜底 |
| R-6 | **`Present1` 兼容性**（首帧全量、空/越界矩形、NULL 指针） | 首帧黑屏或进程终止 | F-72 的三条 + `fresh` 状态机 + `plan` 的夹紧；`present` 里 `debug_assert!(n > 0 || full)` |
| R-7 | **compositor 性能**（每 damage 矩形一次 draw 循环；16 矩形 × 8 段 = 128 draw） | 脏区多时 CPU 录制变慢 | damage 阈值 16/50% 已有（P06）；每帧 draw 录制 ~128 次、可忽略；P18 的 L4 基准量化（`Present`/`Gpu` 段已有采样） |
| R-8 | **device lost 后仍被调用**（错误对象被吞） | 崩溃/UB | `lost` 粘性 + `submit` 入口早退 + 单测（`RemoveDevice` 后二次 submit 返回同一错误） |
| R-9 | **`Win32_Security` feature 遗漏** | 编译失败（E0425） | F-79 + 门禁 §10.2-5 检查 Cargo.toml 的 feature 行 |

---

## 12. 需裁决项（5 项，含建议）

| # | 议题 | 建议 | 备选与代价 |
|---|---|---|---|
| D-1 | `FrameIndex` 放哪：brief 说 `render/frame.rs`，但它已是 `window.rs` 的**公开**类型（`Window::frame_index()` 的返回类型） | **留在 `window.rs`**；`render/frame.rs` 只放 `MAX_FRAMES_IN_FLIGHT`/`slot`/`InFlightRing`（F-74） | 搬到 `render/frame.rs` 并 `pub use`：破坏 P06 的公开路径与 6 处既有测试，且 `render` 私有（I9）要求公开类型不能住那里 |
| D-2 | 绘制到哪：主设计 §2.4 的「copy 到 back + 逐 scissor 画到 back」vs 「只画 persist + copy 到 back」 | **只画 persist**（§0 #3）：少一遍 draw，且 back buffer 不持 RTV（resize 前提） | 按主设计：每次 damage 画两遍（persist + back），GPU 负载翻倍，且要在 swapchain buffer 上建 RTV |
| D-3 | swapchain / persist 的格式组合 | **swapchain `B8G8R8A8_UNORM` + persist 资源 `*_UNORM` + persist RTV 视图 `*_UNORM_SRGB`**：编码一次、copy 同格式合法 | sRGB swapchain：**本机创建即失败**（§2.3 的 `0x887A0001`），不是可选项；persist 用 sRGB 资源：与 UNORM swapchain 不同格式，`CopyTextureRegion` 非法 |
| D-4 | `FrameOp::Geometry` 在 P08（无 `GeometryStore`）的处置 | **返回 `EngineError::GeometryRegister`**（新增变体，P01 表列 P03 无生产者）+ test-only 注册以覆盖 geometry PSO 的像素路径 | 静默跳过：违反 I2；P08 落完整 store：超出本阶段范围且会引入公开注册 API |
| D-5 | `commit_frame` 的返回类型 | **`Result<FrameOutcome, EngineError>`**（GPU 失败必须冒泡） | 保持 `FrameOutcome` + 在 `Window` 存错误：错误有「上次的」二义性，且调用方可能继续用坏状态 |

---

## 13. 本阶段追加到 `design/DEVIATIONS.md` 的条目（§十三）

> 设计期条目（下表）与**实现期新增**条目（F-81…F-85、S-76…S-78、V-56…V-59）都记在
> `design/DEVIATIONS.md` §十三；实现期事实以该文件的 13.3 / 13.4 为准。

| 编号 | 内容 |
|---|---|
| **F-70** | 主设计 §2.4 的 `CopySubresourceRegion1` 在 **D3D12 不存在**（那是 D3D11.3 `ID3D11DeviceContext1`）；`windows 0.62.2` 只暴露 `CopyTextureRegion`。P08 用 `CopyTextureRegion` + `D3D12_BOX`，区域求交在共享层 `render/compositor.rs`（不用 Win32 `IntersectRect`） |
| **F-71** | 帧结构改为「**persist 是唯一绘制目标**，swapchain back buffer 只作 `COPY_DEST`（不建 RTV、不逐 scissor 画到 back）」；主设计 §2.4 的「copy 到 back 再画」被取代 |
| **F-72** | `Present1` 三条**实现期实测**约束：①创建后/`ResizeBuffers` 后第一次 present 必须全量；②dirty rect 非空且完全在 surface 内；③`pPresentParameters` 不得为 NULL。违反 ①② → `0x887A0001`，违反 ③ → 进程终止 |
| **F-73** | `D3D12_FEATURE_DATA_SHADER_MODEL.HighestShaderModel` 是**入参**，必须先填期望上限（6_8）再查；填 `6_0` 会得到 `6_0`（本机实测会把 RTX 5080 误判为 SM 6.0） |
| **F-74** | `FrameIndex` 留在 `window.rs`；`render/frame.rs` 只有 `MAX_FRAMES_IN_FLIGHT` / `slot` / `InFlightRing`（D-1） |
| **F-75** | `SnapshotError` 增 `Gpu { detail: SmolStr }`；`TestApp::snapshot` 在 `cfg(windows)` 用 WARP 真渲染，其余平台仍 `NoRenderer` |
| **F-76** | `Window::commit_frame` 返回 `Result<FrameOutcome, EngineError>`（GPU 错误冒泡；`App::frame` 的 `Some(Err)`） |
| **F-77** | 新增 `EngineError::GeometryRegister { detail: SmolStr }` 与其**首个生产者**（`FrameOp::Geometry` 的 `GeometryId` 未注册；P09 的 `GeometryStore` 接上后语义不变） |
| **F-78** | `InstanceBuffer` 追加 `ops()/geometry()/clips()` 与 `ClipChain::gpu_entry()`、`GpuClipEntry`（48 B）；P06 遗留 ④ 收口（非 test 消费者 = `render/windows/compositor.rs` 的录制循环） |
| **F-79** | `crates/kytrix_gui/Cargo.toml` 的 windows features 追加 **`Win32_Security`**（`CreateEventW` 在 0.62.2 被该 feature 门控；缺了是 `E0425`）。**只改 feature 列表**，无新 crate，`Cargo.lock` 零变化 |
| **F-80** | persist 资源格式 = `B8G8R8A8_UNORM`、**RTV 视图格式 = `B8G8R8A8_UNORM_SRGB`**（编码一次；probe：`clear(0.5) → 188`）；**sRGB flip swapchain 在本机创建即失败**（`0x887A0001`）⇒ swapchain 固定 `*_UNORM`；`CopyTextureRegion` 的源/目标必须同格式，`GetCopyableFootprints` 用**资源格式**。主设计 §2.4 与 P07 §2.3 都未涉及「资源格式 vs 视图格式」与 flip swapchain 的格式限制 |
| **S-73** | `render/windows.rs` 是模块根（只 `mod` + `pub(crate) use`）；`Renderer` 定义在 `windows/compositor.rs`（lint `definition_in_module_root`） |
| **S-74** | **不启用** SM 6.6 `ResourceDescriptorHeap` / Tier3 加速：WARP 报 SM 6.8/Tier3，启用会让 CI golden 与真机走不同 shader 路径（主设计 §6.4 的同一条警告） |
| **S-75** | descriptor 堆**静态分区**（每 in-flight 槽 4 个 SRV + 2 个备用，无动态索引回收）；`GpuResourceHandle` 的 `kind=2/3` 编码为 P09 预留，P08 只产生 `kind=1` |
| **V-55** | 「present 之后的 `ResizeBuffers`」在**本机非交互会话 + 隐藏窗口**下无法验证（`0x887A0001`，含 waitable/泵消息/释放全部引用三种尝试）；P08 以纯逻辑单测 + 「失败即 `EngineError::Swapchain`、不重试」处置，**P09 必须在可见窗口会话补实机验证** |

---

## 附录 A · 实现检查单（按执行顺序）

1. `Cargo.toml`：windows features + `Win32_Security`（F-79）；`Cargo.lock` 零变化。
2. `render/{device,frame,retire,compositor,timer}.rs` + 单测（无 GPU）；`render.rs` 补 `mod` 行。
3. `paint/instance.rs`：`GpuClipEntry` + `gpu_entry()` + `ops()/geometry()/clips()`（F-78）；`error.rs` 加 `GeometryRegister`。
4. `render/windows/{device,queue}.rs`：WARP 设备 + ring + fence + retire；单测（caps/ResourceTable/lost）。
5. `render/windows/pipeline.rs`：根签名（184 B）→ quad PSO → geometry PSO；单测（WARP 可创建）。
6. `render/windows/{atlas,readback}.rs`：纹理上传 + 脏行 + 读回；单测（WARP 像素 + pitch）。
7. `render/windows/{compositor,swapchain,timer}.rs`：帧编排 + `Present1`（`fresh` 状态机）+ timestamp。
8. `window.rs` / `app.rs` 接线（`Option<Renderer>`、`commit_frame` → `Result`）；既有 418 项单测必须**逐项不变**。
9. `test_support` 快照（F-75）；`--features test-support` 集成测试。
10. 跑 §10.2 的 5 组门禁；三平台 check；DEVIATIONS §十三。

## 附录 B · probe 复现

```powershell
# 目录：%TEMP%\kg-p08-d3d12（Cargo.toml 依赖 windows 0.62.2；shaders\ 从仓库 render\shaders 复制）
cd "$env:TEMP\kg-p08-d3d12"
$env:CARGO_TARGET_DIR = "$env:TEMP\kg-p08-d3d12\target"
cargo run --offline          # 每行以 [P08] 开头；exit=0
```

| 段 | 关键输出（本机） |
|---|---|
| caps | `hardware FL=12_2 SM=6_8 tier=3` / `warp FL=12_1 SM=6_8 tier=3` |
| clear/readback | `rgb [137,188,225]`、`row_pitch=1024 total=262144` |
| quad | `center=[255,0,0] ul=红 lr=红 outside=[0,0,0]`；`TRIANGLELIST` 负例 `center=[0,0,0]` |
| persist | `frame2: blue(140,140)` + `persisted red(30,30)` |
| timestamp | `freq=1000000000 delta≈10.5 µs`（64 clears） |
| ring | `8 frames / 3 allocators, final fence = 19` |
| swapchain | `Present1(dirty)` 首次 `0x887A0001` → 全量后 `S_OK`；越界/空矩形 `0x887A0001`；NULL 参数 AV |
| resize | 未 present：`S_OK`；已 present：`0x887A0001`（waitable/泵消息均无效） |
| 格式互操作 | `UNORM 资源 + SRGB RTV 视图: clear(0.5) → [188,188,188]`；`sRGB flip swapchain: creation failed 0x887A0001` |
| lost | `RemoveDevice` → `0x887A0005`；DRED `S_OK`；lost 后 present `0x887A0005` |

## 附录 C · 与上游文档的交叉引用

| 本文 | 上游 |
|---|---|
| §2.2 D8 清单 | 主设计 §2.2 的 8 项表 |
| §4.4 `plan` | 主设计 §5.7、P02 §12（damage 合并规则）、P06 §8（`DamageSummary`，F-61） |
| §5.4 根签名/PSO | P07 §2.7（`RootUniform` 112 B）、§2.9（绑定表 / `TriangleStrip` / 4 顶点 / cull NONE）、F-68 |
| §5.5 图集上传 | P02 §10.2（UV 边约定）、§10.3（格式/padding）、§10.5（`DirtyRows`）、P05 §9 |
| §5.6 帧流程 | 主设计 §2.4 / §2.7、P06 §9（`commit_frame`/`FrameIndex`） |
| §7.3 上传访问器 | P06 §7.7 / §7.8、F-78 / S-72 |
| §7.7 快照 | 主设计 §6.4（L3）、P06 §10.6（`SnapshotCase`）、P07 §7.4（容差） |
| §8 retire | 主设计 §2.7（`RetireQueue` 草图） |
| §11 R-5 / V-55 | 主设计 §2.9（resize 不重建 VBO/实例容量） |
