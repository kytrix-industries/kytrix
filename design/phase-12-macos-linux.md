# Phase 12 · macOS（Metal）+ Linux（Vulkan / Wayland）后端 — 详细设计（v1）

> 依据：`custom-gui-engine-design.md` §2.1 / §2.2 / §2.5 / §2.6 / §2.7 / §2.8 / §2.9、§2.10、§8.1 / §8.2 / §8.3、
> §7.6、§9.4；`design/phase-08-d3d12.md`（共享层契约 / `FramePlan` / retire / readback / `Present1` 语义）；
> `design/phase-09-win32-demo.md`（平台层形状 / `Dispatcher` / `Event` 映射）；`design/phase-11-a11y.md` §12（P12 交接）；
> 已落地 `render.rs` / `render/{device,frame,retire,compositor,timer,atlas,damage}.rs`、`render/windows/**`、
> `platform.rs`、`platform/{dispatcher,event}.rs`、`platform/windows/**`、`render/shaders/*.{metal,glsl}` 与 `build.rs`；
> P01 附录 A（objc2 / wayland / xkbcommon 版本与 feature）；DEVIATIONS（V-04 / S-78 / F-70…F-118 / V-55 / V-76）。
>
> 本文 §N 指本文件；**本轮只做设计 + 本机交叉 target 编译 probe**，不写仓库代码、不改 `Cargo.toml` / `Cargo.lock`、
> 不 commit。probe 产物全部在 `%TEMP%\kg-p12-macos-linux`（仓库外），复现见附录 A。

---

## 0. 结论摘要（先给定死项）

| # | 定死项 | 一句话依据 |
|---|--------|-----------|
| 1 | **两个 target 的 `cargo check` 是唯一静态门禁**：`aarch64-apple-darwin` 现在 **exit 0**；`x86_64-unknown-linux-gnu` 在**未配 pkg-config** 时卡在 `yeslogic-fontconfig-sys` 的 build script（build script 进程 exit 101 ⇒ `cargo check` overall exit 1），配 12 行 stub `pkg-config` 后 **exit 0**（本轮实测）。P12 实现后两 target 必须仍 exit 0 | §2.2；DEVIATIONS V-04 复验 |
| 2 | **同形目录、零 trait / 零枚举分派**：`render/{macos,linux}/**` + `platform/{macos,linux}/**`，`render.rs` / `platform.rs` 用 `#[cfg(target_os)] pub(crate) use … as backend/native` 编译期选一份（I14） | §3.1 / §3.2 |
| 3 | **八个类型同名同形**：`Device` / `Queue` / `Swapchain` / `Pipeline` / `AtlasTextures` / `Renderer` / `Readback` / `GpuTimer`，方法名与 `render/windows/**` 逐一对齐；共享 `GpuCaps` 语义、`FramePlan`、`RetireQueue`、`InFlightRing`、`GpuResourceHandle`、`RootUniform`(112 B)、`GpuClipEntry`(48 B)、实例布局(128 B)、`ReadbackImage`(RGBA8) | §3.3 / §4 |
| 4 | **persist 是唯一绘制目标；每帧整面写满 present 目标**：drawable / swapchain image 的内容**跨帧无保证**，所以 Metal 每帧 `blit` 整面、Vulkan 每帧 `cmd_copy_image` 整面；damage 只用于 **scissor 省 draw**，不用于省 copy/present | §5.9 / §6.9；F-121 |
| 5 | **Metal 不做 argument buffer**：v1 与 P07 已落地的 MSL 契约一致（`buffer(0/1/2)` + `texture(0/1)` + `sampler(0)` 直绑）。`argumentBuffersSupport == Tier2` 仍作为 D8 检测项（前置能力 + 未来多页图集的门），但**不是** v1 的绑定模型 | §5.10；F-119 |
| 6 | **Metal 同步 = `waitUntilCompleted`**：与 D3D12 的「Signal + 等 fence」同形（同步提交）；`MTLSharedEvent` 的 `encodeWaitForEvent:` 需要 `&ProtocolObject<dyn MTLEvent>` 上转，objc2 0.6.5 **没有**该上转（probe 实测编译失败），故 v1 不用；异步化留给 P13 | §5.2；F-120 |
| 7 | **Metal 加载 metallib 用 `newLibraryWithData:error:`**，需要 objc2-metal 的 `dispatch2` feature + `dispatch2` 作为**直接依赖**（`dispatch2 0.3.1` **已在 `Cargo.lock`**，objc2-metal 的 default features 已启用该 feature ⇒ **零 lock 变化**） | §5.4；F-122；D-2 |
| 8 | **Vulkan persist 与 swapchain 同格式**（`B8G8R8A8_SRGB`，否则 `R8G8B8A8_SRGB`），因此 `vkCmdCopyImage` 合法且 sRGB 编码只发生一次（写入 persist 时）；readback 取到的是**编码后字节**，与 D3D12 同一契约 | §6.3 / §6.6；主设计 §2.6 |
| 9 | **Vulkan 用固定 descriptor set（4 个 binding）+ 112 B push constants**，与 P07 的 GLSL 契约一致；`VK_EXT_descriptor_indexing` 在 v1 **不使用** runtime array（`instances[]` 是 std430 结构体数组，不是 descriptor array） | §6.4；F-123；D-4 |
| 10 | **`VK_KHR_incremental_present` 只作 hint**：可用则把 `VkPresentRegionsKHR` 挂 `pNext`，不可用/被 presentation engine 忽略都**不影响正确性**（因为每帧整面 copy）；引擎**不依赖**它生效 | §6.3；主设计 §2.6 |
| 11 | **平台层：macOS = `NSApplication` + `CFRunLoopSource` 唤醒 + `CFRunLoopTimer`；Linux = `calloop::EventLoop` + `WaylandSource` + `calloop::ping`**。macOS **不**用 `NSApplicationDelegate` 以外的 run loop 改写、**不**用 libdispatch（无 `dispatch_async` 依赖）；唤醒一律「先置位再 signal」，与 Win32 的 `PostMessageW` 合并策略同构 | §7.1 / §8.1 |
| 12 | **公共 `Event` / `InputEvent` / `ImeEvent` 是三平台唯一事件契约**；`platform/event.rs`（含 `translate` / `PlatformEvent`）**保持 Windows 专用不重构**，mac/Linux 各自的 `event.rs` 直接构造公共 `Event` 后调 `App::dispatch` | §3.2；§7.8 / §8.8 |
| 13 | **不新增 `EngineError` 变体**：`UnsupportedGpu` / `DeviceLost` / `WaylandCompositor` / `ShaderLoad` / `Swapchain` / `AppInit` / `WindowCreate` / `Clipboard` / `InvalidDpiScale` / `A11yInit` 已覆盖 P12 全部失败路径 | §9.1 |
| 14 | **a11y 三平台直连**（§7.6 / D22）：macOS `accesskit_macos::Adapter::new(*mut c_void, …)`（`unsafe`、**主线程**）、`hit_test` / `view_children` / `focus` 返回**裸指针**且点是 **objc2 0.2 线**的 `NSPoint`；Linux `accesskit_unix::Adapter::new(activation, action, deactivation)`，`request_initial_tree → None` 合法（进 `Pending`），`update_if_active` **无返回值** | §7.7 / §8.7；F-124 |
| 15 | **Cargo 零新增依赖**：唯一直依赖增量是 `dispatch2 = "0.3.1"`（已在 lock、已由 objc2-metal default 启用）——列为 D-2 裁决；不引入 `block2` / `dispatch2` 之外任何 crate，不加 `x11` / `system` / `wayland-cursor` | §2.5 / §12 |
| 16 | **无 GLES / 无 X11 / 无 winit**（I16 / D25）：Linux 只走 Wayland，`cargo tree` 不得出现 `x11*` / `xcb*` / `winit`。**实现更正**：`wayland-client` 必须开 `system`（Vulkan WSI 要真实 `wl_display*`/`wl_surface*`，纯 Rust 后端只有令牌），因此 `wayland-sys` + `libwayland-client` 必然出现在树里（F-135） | §11.2 |

---

## 1. 范围、文件与非目标

### 1.1 本阶段交付（P12 实现期落地的文件）

```text
crates/kytrix_gui/src/
├── render.rs                                  # 修改：解除 #[cfg(windows)]、加 macos/linux 模块根
├── render/
│   ├── device.rs                              # 修改：D-1 拆分（见 §4.1），本文件降到「三平台 *REQUIRED 常量」
│   ├── macos.rs                               # 新增：模块根（只 mod + pub(crate) use Renderer）
│   └── macos/
│       ├── device.rs                          # MTLDevice / GpuCaps / D8 / lost / ResourceTable
│       ├── queue.rs                           # MTLCommandQueue + 三槽 in-flight + 同步提交
│       ├── swapchain.rs                       # CAMetalLayer / nextDrawable / 整面 blit / resize
│       ├── pipeline.rs                        # metallib / quad+geometry PSO / sampler / blend
│       ├── atlas.rs                           # R8 与 BGRA8_sRGB 图集 / 脏行 / generation
│       ├── compositor.rs                      # Renderer::submit（帧编排，唯一 unsafe 汇聚点）
│       ├── readback.rs                        # Private → blit → Shared → getBytes
│       └── timer.rs                           # GPUStartTime/GPUEndTime
├── render/
│   ├── linux.rs                               # 新增：模块根
│   └── linux/
│       ├── instance.rs                        # VkInstance / VK_KHR_wayland_surface / surface
│       ├── device.rs                          # 选卡 / GpuCaps / D8 / features / queue / timeline / fence
│       ├── swapchain.rs                       # Wayland WSI / formats / acquire / present / hint / resize
│       ├── pipeline.rs                        # descriptor set layout / push constants / 3 pipeline
│       ├── atlas.rs                           # image+view+sampler / staging / barrier / 上传
│       ├── compositor.rs                      # Renderer::submit + barrier 表
│       ├── readback.rs                        # host-visible buffer + copy → map
│       └── timer.rs                           # vkCmdWriteTimestamp
├── platform.rs                                # 修改：解除 #[cfg(windows)]、加 macos/linux
├── platform/
│   ├── macos.rs                               # 新增：模块根
│   ├── macos/
│   │   ├── event_loop.rs                      # NSApplication + CFRunLoopSource + CFRunLoopTimer + 泵
│   │   ├── view.rs                            # NSView 子类 + CAMetalLayer + NSTextInputClient + NSAccessibility
│   │   ├── window.rs                          # NSWindow / 交通灯 / 标题栏拖拽 / 最小尺寸
│   │   ├── dpi.rs                             # backingScaleFactor → Scale
│   │   ├── ime.rs                             # NSTextInputClient → ImeEvent 状态机
│   │   ├── clipboard.rs                       # NSPasteboard
│   │   └── a11y.rs                            # accesskit_macos 直连
│   ├── linux.rs                               # 新增：模块根
│   └── linux/
│       ├── event_loop.rs                      # calloop + registry + 必需 global + ping 唤醒
│       ├── surface.rs                         # xdg_toplevel / configure / ack / frame / fractional scale
│       ├── window.rs                          # 窗口尺寸 / 最小尺寸 / move / resize / 标题栏拖拽
│       ├── dpi.rs                             # 分数缩放 → Scale
│       ├── ime.rs                             # zwp_text_input_v3 状态机
│       ├── clipboard.rs                       # wl_data_device / wl_data_source + 缓存
│       └── a11y.rs                            # accesskit_unix 直连
└── window.rs / app.rs                         # 修改：cfg 解除（见 §3.3）
```

**零新增**：`Cargo.toml` 的 target 依赖段**不改**（P01 已声明齐全，probe 用同一批 feature 编译通过）；唯一可选增量是
`dispatch2` 直依赖（D-2）。`Cargo.lock` 零变化。

### 1.2 明确的非目标

| 不做 | 归属 |
|------|------|
| macOS / Linux 的**运行时**验证（真机出帧、像素回归、L5 实机矩阵） | L5 / P18（V-82） |
| 异步提交 / `MTLSharedEvent` 等待编码 / 多线程录制 | P13（F-120） |
| Metal argument buffer 承载纹理数组 / Vulkan runtime descriptor array | 未来阶段（F-119 / F-123） |
| Linux X11 / XWayland / GLES 回退 | D25 / I16，永久不做 |
| Wayland 服务端装饰、窗口圆角/阴影的系统侧效果 | 不做（自绘标题栏 + 客户端装饰） |
| mac/Linux 的 `TestApp::snapshot` 真实像素 | P18（P12 只编译 `readback.rs`，见 §11.1） |
| 新 `EngineError` 变体 / 新 crate / `serde` 面 | 不需要 |

---

## 2. probe（本机，2026-11-13）

### 2.1 环境

| 项 | 值 |
|---|---|
| OS / 宿主 | Windows，rustc **1.97.1**（stable），`cargo 1.97.1`；registry `rsproxy.cn` |
| 已装 target | `x86_64-pc-windows-msvc`、**`aarch64-apple-darwin`**、**`x86_64-unknown-linux-gnu`** |
| macOS 工具链 | **无** `xcrun` / `metal` / `metallib`（`build.rs` 走 `KYTRIX_GUI_SKIP_SHADERS=1` 的占位分支） |
| Linux 工具链 | **无** `glslangValidator`、**无** sysroot、**无** pkg-config |
| 实机 | 无 macOS / Linux 机器（V-82：全部运行期结论不可得） |

### 2.2 两个 target 的 `cargo check` 基线（本轮实测）

```powershell
$env:KYTRIX_GUI_SKIP_SHADERS = "1"
cargo check -p kytrix_gui --target aarch64-apple-darwin          # → exit 0（60 warnings，全是 dead_code）
cargo check -p kytrix_gui --target x86_64-unknown-linux-gnu      # → exit 1（第三方 build script 卡住）
```

| target | 结果 | 阻塞点 | 解法 |
|---|---|---|---|
| `aarch64-apple-darwin` | **exit 0**，`kytrix_gui (lib) generated 60 warnings`，`Finished dev profile` | 无（`objc2*` / `accesskit_macos` 全是纯 Rust 绑定，`cargo check` 不链接） | — |
| `x86_64-unknown-linux-gnu` | **exit 1**，`failed to run custom build command for yeslogic-fontconfig-sys v6.0.1`，`pkg-config has not been configured to support cross-compilation` | `fontique 0.12` → `yeslogic-fontconfig-sys 6.0.1` 的 `build.rs:8` `pkg_config::find_library("fontconfig")`；与本阶段代码无关（P07/P08 起既有） | 12 行 rustc 编译的 stub `pkg-config`（见下），`PKG_CONFIG=<stub>` 后 **exit 0**，同样 60 warnings |
| 两 target + `--features test-support` | **exit 0 / exit 0**，各 **28 warnings** | 无 | 同一套环境变量；这条覆盖 `readback.rs`（默认 check 里它被 `#[cfg]` 排除） |

**stub `pkg-config`（实测可用；仓库外，`%TEMP%\p12-probe\pkg-config.exe`）**：

```rust
// 只回答 pkg-config-rs 0.3.34 会问的三种调用：--libs [--cflags]、--modversion
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut libs, mut cflags, mut modver) = (false, false, false);
    for a in &args {
        match a.as_str() {
            "--libs" | "--libs-only-l" | "--libs-only-L" | "--libs-only-other" => libs = true,
            "--cflags" | "--cflags-only-I" | "--cflags-only-other" => cflags = true,
            "--modversion" => modver = true,
            _ => {}
        }
    }
    if modver { println!("2.14.0"); return; }
    if cflags { print!("-I/stub/include "); }
    if libs { print!("-lfontconfig"); }
    println!();
}
```

```powershell
rustc -O pkg-config.rs -o pkg-config.exe
$env:PKG_CONFIG = "$env:TEMP\p12-probe\pkg-config.exe"
$env:PKG_CONFIG_ALLOW_CROSS = "1"     # 有了 PKG_CONFIG 也可省（pkg-config-rs 见 PKG_CONFIG 即假定已配好交叉）
cargo check -p kytrix_gui --target x86_64-unknown-linux-gnu   # exit 0
```

**不采用的替代（实测不可行）**：`RUST_FONTCONFIG_DLOPEN=1` 会让 `yeslogic-fontconfig-sys` 编进去 `feature="dlopen"`，
但 `fontique 0.12` 依赖的 `fontconfig_sys::{FcCharSetAddChar, FcConfigBuildFonts, …}` 在 dlopen 形态下**不在根导出**
（`E0432` 29 个符号），所以在 Windows 宿主上不能用它绕过（记录在 F-125）。

**60 warnings 的性质**（两 target 相同）：全部是 `never used`（`App` / `Window` / `Dispatcher` / `a11y` 的平台消费者在
`#[cfg(windows)]` 里，非 Windows target 没有泵，所以整条链无调用者）。**这不是 P12 引入的**；P12 落地后
`platform::{macos,linux}` 成为 `native`，这些 item 重新获得非 test 消费者，warning 数应显著下降。
门禁判据（§11.2）：**两 target `check` exit 0，且 `render/{macos,linux}` / `platform/{macos,linux}` 内不得出现
`never used` 告警**（`#[cfg]` 外的 share 层老告警不计）。

### 2.3 probe crate（仓库外，`%TEMP%\kg-p12-macos-linux`）

一个 target-切分的 probe crate，依赖**逐字复制 P01 的声明**（`objc2 0.6.5` / `objc2-metal 0.3.2` / `objc2-app-kit 0.3.2` /
`objc2-quartz-core 0.3.2` / `objc2-core-foundation 0.3.2` / `accesskit 0.25.2` / `accesskit_macos 0.27.2`；
`ash 0.38.0` / `wayland-client 0.31.15` / `wayland-protocols 0.32.13`(client,staging,unstable) / `calloop 0.14.5` /
`calloop-wayland-source 0.4.1` / `xkbcommon 0.9.0`(wayland) / `accesskit_unix 0.24.1`(tokio)），
`src/macos.rs` 与 `src/linux.rs` 把本文 §5–§8 将要写的每个原生调用**先编译一遍**。

| probe | 命令 | 结果 |
|---|---|---|
| macOS | `cargo check --target aarch64-apple-darwin` | **exit 0** |
| Linux | `cargo check --target x86_64-unknown-linux-gnu` | **exit 0** |

probe 只做 `check`（不链接、不运行），因此不依赖 Xcode / Wayland / Vulkan 运行时。附录 B 是编译验证过的签名速查表。

### 2.4 不可行 / 受限项（P12 登记）

| # | 限制 | 处置 |
|---|---|---|
| 1 | 本机**无** `xcrun metal` / `glslangValidator`：MSL / SPIR-V **未编译**，`OUT_DIR/shaders/*.metallib` / `*.spv` 是 `KYTRIX-SHADER-SKIPPED` 占位 | 加载器必须显式识别占位（§5.4 / §6.4 的 `ShaderLoad`）；CI 侧补工具链（§12 R-2） |
| 2 | 本机**无** macOS / Wayland 运行时：`CAMetalLayer.nextDrawable`、`vkQueuePresentKHR`、xkb keymap、IME、a11y 全部不可验证 | L5 实机矩阵（V-82） |
| 3 | `MTLArgumentBuffersTier` 的真实取值不可得 | `argumentBuffersSupport()` 走设备属性，D8 判定按 API 值（§5.1） |
| 4 | Linux 交叉 `check` 需要 stub `pkg-config`（第三方 build script 的交叉限制） | 门禁命令固化（§11.2）；Linux CI 原生构建装 `libfontconfig-dev` / `libxkbcommon-dev`（V-04） |

---

## 3. 模块结构与边界

### 3.1 文件树与非 test 消费者（dead_code 审计表）

`render.rs`（模块根，**只**允许 `mod` / `pub(crate) mod` / `pub(crate) use`）：

```rust
//! 渲染共享层（crate 私有；应用层不可见，I9）。
pub(crate) mod atlas;
pub(crate) mod compositor;
pub(crate) mod damage;
pub(crate) mod device;          // 只留三个 *_REQUIRED 常量（§4.1）
pub(crate) mod frame;
pub(crate) mod retire;
pub(crate) mod timer;

#[cfg(windows)]
pub(crate) mod windows;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(target_os = "linux")]
pub(crate) mod linux;

#[cfg(windows)]
pub(crate) use windows as backend;
#[cfg(target_os = "macos")]
pub(crate) use macos as backend;
#[cfg(target_os = "linux")]
pub(crate) use linux as backend;

pub(crate) use atlas::{Allocation, AtlasAllocator, AtlasSet, DirtyRows};
```

| item | 定义 | 非 test 消费者 |
|------|------|----------------|
| `MAX_FRAMES_IN_FLIGHT` / `FrameSlot` / `InFlightRing` | `render/frame.rs` | `render/{windows,linux}/queue.rs`（Metal 用 `[u64;3]` 自有记账，见 §5.2） |
| `RetireQueue` / `GpuResourceHandle` | `render/retire.rs` | 三后端的 `Queue` / `Device::drain_retire` |
| `Plan` / `DamageRects` / `FramePlan` / `plan` | `render/compositor.rs` | `window.rs::commit_frame` + 三后端的 `submit` |
| `Section` / `GpuTimerSample` / `FrameTimings` | `render/timer.rs` | 三后端的 `Renderer::submit` + 各自 `timer.rs` |
| `GpuClipEntry` / `ClipChain::gpu_entry` / `InstanceBuffer::{as_bytes,ops,geometry,clips}` | `paint/instance.rs`（P08 已落） | 三后端的录制循环 |
| `Renderer` | `render/{windows,macos,linux}/compositor.rs` | `window.rs::renderer` |
| `Device` / `Queue` / `Swapchain` / `Pipeline` / `AtlasTextures` / `Readback` / `GpuTimer` / `GpuCaps` | 各后端目录 | 同目录的 `Renderer`（唯一入口，不跨模块泄漏） |

**可见性纪律**（沿用 P08 §3.1）：`render/` 整体 `mod render;`；`pub(crate)` 的每一项必须在上表里有一个**非 test** 消费者。
`render/{macos,linux}/**` 内部私有项只需同模块消费。`readback.rs` 与 `Renderer::readback` 只在
`#[cfg(any(test, feature = "test-support"))]` 下编译（与 Windows 完全一致），因此发布物不含读回路径。

`platform.rs`：

```rust
//! 平台层（crate 私有；应用层不可见，I9）。
pub(crate) mod dispatcher;

#[cfg(windows)]
pub(crate) mod event;            // 保持 Windows 专用（§3.2）
#[cfg(windows)]
pub(crate) mod windows;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(target_os = "linux")]
pub(crate) mod linux;

#[cfg(windows)]
pub(crate) use windows as native;
#[cfg(target_os = "macos")]
pub(crate) use macos as native;
#[cfg(target_os = "linux")]
pub(crate) use linux as native;
```

### 3.2 无 trait / 无枚举分派（I1 / I14）

- **没有** `trait RenderBackend` / `Box<dyn EventLoop>` / `enum Backend`；`render.rs` / `platform.rs` 的 `use … as backend/native`
  是唯一选择点，`#[cfg(target_os)]` 保证每个 target 恰好一份实现。
- **不重构 `platform/event.rs`**：它（`PlatformEvent` / `PointerKind` / `TranslateState` / `translate` / `key_from_vk` /
  `loword_signed`）整块依赖 Win32 常量与 `platform/windows/ime.rs::ImeState`，**保持 `#[cfg(windows)]`**。
  macOS / Linux 的 `platform/*/event.rs`（或 `event_loop.rs` 内联）直接把原生事件翻成公共
  `Event` / `InputEvent` / `ImeEvent` 后调 `App::dispatch(window, event)`——与 Win32 的
  `PlatformEvent → Event` 转换（`platform/windows/event_loop.rs::to_app_event`）是**同一层职责**，只是少了中间枚举。
  ⇒ 公共 `Event` 是唯一跨平台契约（结论 #12）。
- 唯一的 trait object 仍是 `Dispatcher` 的唤醒回调（`Arc<dyn Fn() -> bool + Send + Sync>`），平台侧只是换成
  「CFRunLoopSource signal」或「calloop ping」。

### 3.3 与既有代码的接线（签名变更，逐字）

```rust
// render.rs / platform.rs：见 §3.1（纯 cfg 变更）

// window.rs
#[cfg(windows)]
use crate::render::Renderer;
// →
use crate::render::Renderer;                       // 三平台都有后端，删除 cfg

pub struct Window {
    // …既有字段
    renderer: Option<Renderer>,                     // 删除 #[cfg(windows)]
}

impl Window {
    pub(crate) fn set_renderer(&mut self, renderer: Renderer);          // 删除 cfg
    pub(crate) fn has_renderer(&self) -> bool;                          // 删除 cfg
    pub(crate) fn enable_offscreen_renderer(&mut self) -> Result<(), EngineError>;   // 删除 cfg
    pub(crate) fn queue_atlas_upload(&mut self, kind: AtlasKind, pixels: &[u8], rows: DirtyRows); // 删除 cfg
    fn submit_gpu(&mut self, renderer: &mut Renderer, index: FrameIndex, damage: DamageSummary)
        -> Result<(), EngineError>;                                     // 删除 cfg
}

// app.rs
pub struct Application;                             // 删除 #[cfg(windows)]（§7.1 / §8.1）
impl Application { pub fn run(self, app: App) -> Result<(), EngineError>; }   // 删除 cfg

impl App {
    pub fn set_clipboard_text(&self, text: &str) -> Result<(), EngineError>;  // 删除 cfg，改走 native::clipboard
    pub fn clipboard_text(&self) -> Result<SmolStr, EngineError>;             // 同上
    pub(crate) fn set_native_window(&mut self, handle: isize);                // 删除 cfg（mac: NSView*，linux: 0）
    pub(crate) fn attach_a11y(&mut self, window: WindowId);                   // 删除 cfg
    pub(crate) fn upload_glyph_page(&mut self, window: WindowId);             // 删除 cfg（三后端同形）
}
```

`Window::commit_frame` / `App::frame` 的语义**不变**：`renderer.submit(...)` 的 `Err` 继续冒泡为 `Some(Err(..))`，
`FrameIndex` 已递增 ⇒ 不重试本帧。`set_size` / `set_scale` 继续只置 `full_damage_pending`，真正 resize 在下一次
`submit` 的 `begin`。

**`App::set_clipboard_text` 的实现变更**（逐字）：

```rust
pub fn set_clipboard_text(&self, text: &str) -> Result<(), EngineError> {
    crate::platform::native::clipboard::set_text(text)     // 三平台同名函数
}
pub fn clipboard_text(&self) -> Result<SmolStr, EngineError> {
    crate::platform::native::clipboard::get_text()
}
```

---

## 4. 共享层（P12 只动这些）

### 4.1 `render/device.rs` 的拆分（D-1）

**问题**：现有 `GpuCaps` 的 14 个字段**全部是 D3D12 语义**（`feature_level` / `shader_model` / `resource_binding_tier` /
`tiled_resources_tier` / `max_sampler_descriptor_heap` …）。Metal 需要的是
`registry_id` / `unified_memory` / `low_power` / `argumentBuffersTier` / `maxBufferLength`；Vulkan 需要
`api_version` / `device_type` / `descriptor_indexing` / `timeline_semaphore` / `incremental_present`。
把三者塞进一个 struct 会得到「每平台 2/3 字段是 0」的结构（违反 AGENTS 1.2，且 `actual()` 无法稳定格式化）。

**定案**：

| 文件 | 内容 |
|---|---|
| `render/device.rs`（共享） | 只留**三个错误文案常量**：`D8_REQUIRED_WINDOWS` / `D8_REQUIRED_MACOS` / `D8_REQUIRED_LINUX`（与安装器、发布说明共用同一份字符串，主设计 §2.2） |
| `render/windows/device.rs` | 原 `GpuCaps` + `fl_name` / `sm_name` + `FEATURE_LEVEL_12_0` / `SHADER_MODEL_6_2` + 5 个既有单测（机械搬移，语义零变化） |
| `render/macos/device.rs` | 新 `GpuCaps`（§5.1），同名方法 `baseline_ok()` / `actual()` / `describe()` / `check()`，`check()` 用 `D8_REQUIRED_MACOS` |
| `render/linux/device.rs` | 新 `GpuCaps`（§6.2），同名四方法，`check()` 用 `D8_REQUIRED_LINUX` |

三个 `check()` 的错误构造完全同形：`EngineError::UnsupportedGpu { os, required, actual }`，`os` =
`"windows"` / `"macos"` / `"linux"`，文案必须同时给「实际值 + 最低要求 + 指向 release notes」，**不得**出现降级措辞（I10）。

### 4.2 `render.rs` 的 `#[cfg(windows)]` 解除（S-78 收口）

P08 的 DEVIATIONS S-78 定的规则是「共享层只在有后端消费者的平台上编译」。P12 让三个 target 都有后端，因此
`render/{atlas,compositor,damage,frame,retire,timer}.rs` **去掉 `#[cfg(windows)]` 无条件编译**（`atlas` / `damage` 本来已经无条件）。
`render/device.rs` 保留（§4.1）。`window.rs` 的 `renderer` 字段同理（§3.3）。

### 4.3 `FramePlan` 在三后端的语义差异（**必须写进代码注释**）

`render/compositor.rs` 的 `FramePlan` 同时承载「绘制范围」「copy 范围」「present 脏集」：

| 访问器 | D3D12（P08） | Metal（P12） | Vulkan（P12） |
|---|---|---|---|
| `plan()` | 逐 damage `RSSetScissorRects` + draw | 同（`setScissorRect`） | 同（`cmd_set_scissor`） |
| `is_full()` | 全量 clear + `CopyResource` | 全量 `MTLRenderPass` clear（`load=Clear`） | 全量 clear（`load_op=CLEAR`） |
| `copy_plan()` | `Partial` 时只 copy 脏矩形 | **不使用**：drawable 内容无保证 ⇒ 每帧整面 blit | **不使用**：swapchain image 内容无保证 ⇒ 每帧整面 copy |
| `present_dirty()` | `Present1` 的 dirty 集 | **不使用**（平台无局部 present） | 只作 `VkPresentRegionsKHR` **hint** |
| `presented()` | `Present1` 成功后清 `fresh` | `presentDrawable` 提交后清 | `queue_present` 成功后清 |

因此 `FramePlan` **不加新方法、不改语义**，由 mac/linux 的 `submit` 只读 `plan()` / `is_full()`，
把 `copy_plan()` / `present_dirty()` 视为 D3D12 专用（`#[allow]` 不需要：它们仍有 Windows 消费者）。
**这是本阶段唯一「同名 API、不同语义」的地方，必须在 `render/macos/compositor.rs` /
`render/linux/compositor.rs` 的文件头注释里逐字写明**（F-121）。

### 4.4 复用清单（P12 **不新增**共享类型）

| 复用 | 位置 | 说明 |
|---|---|---|
| `DamageSummary` / `DamageAccumulator` | `render/damage.rs` | 三平台同源 |
| `Plan` / `DamageRects` / `FramePlan` / `plan()` | `render/compositor.rs` | §4.3 |
| `RetireQueue` / `GpuResourceHandle` | `render/retire.rs` | mac/linux 的 `ResourceTable` 用同一编码 |
| `FrameSlot` / `InFlightRing` | `render/frame.rs` | Vulkan 直接用；Metal 用等价的三槽记账（§5.2） |
| `Section` / `GpuTimerSample` / `FrameTimings` | `render/timer.rs` | 三平台同源 |
| `GpuPrimitive`(128 B) / `GpuClipEntry`(48 B) / `RootUniform`(112 B) | `paint/instance.rs` + `contract.md` | 逐字节同源 |
| `Allocation` / `AtlasAllocator` / `AtlasSet` / `DirtyRows` | `render/atlas.rs` | 三平台同源 |
| `ReadbackImage { width, height, rgba8 }` | `render/windows/readback.rs` | **形状不变**；mac/linux 各自定义同名同字段类型（`pub(crate)`，同模块内消费） |

> 注：`ReadbackImage` 目前在 `render/windows/readback.rs` 里定义。P12 **不**把它提到共享层——它只在
> `test-support` / `test` 构建里存在，mac/linux 的 `readback.rs` 需要自己的同名类型才能与 `Renderer::readback` 同形。
> 若审查要求单一真值，可移到 `render/readback.rs`（可选，不阻塞）。

---

## 5. macOS · Metal 后端规格

> 本节每个 API 都已在 probe 里编译过（§2.3）。`unsafe` 只出现在 `render/macos/**`，每块带 `// SAFETY:`。

### 5.1 `macos/device.rs`

```rust
pub(crate) struct GpuCaps {
    pub adapter: SmolStr,                 // MTLCreateSystemDefaultDevice 的 name（不可得时用 "Apple GPU"）
    pub registry_id: u64,                 // MTLDevice.registryID
    pub unified_memory: bool,             // hasUnifiedMemory
    pub low_power: bool,                  // isLowPower
    pub max_buffer_length: u64,           // maxBufferLength
    pub argument_buffers_tier: i32,       // argumentBuffersSupport().0（0 = Tier1, 1 = Tier2）
}
impl GpuCaps {
    pub(crate) fn baseline_ok(&self) -> bool;   // tier == Tier2（设备存在由 Device::new 保证）
    pub(crate) fn actual(&self) -> SmolStr;     // "ArgumentBuffers Tier 1" / "ArgumentBuffers Tier 2"
    pub(crate) fn describe(&self) -> SmolStr;   // 全字段诊断行（无时间戳）
    pub(crate) fn check(&self) -> Result<(), EngineError>;  // UnsupportedGpu { os: "macos", .. }
}

pub(crate) struct Device {
    raw: Retained<ProtocolObject<dyn MTLDevice>>,
    caps: GpuCaps,
    resources: ResourceTable,             // GpuResourceHandle → Retained<ProtocolObject<dyn MTLResource>>
    lost: Option<EngineError>,            // 粘性：任何 submit 入口早退
}
impl Device {
    pub(crate) fn new() -> Result<Self, EngineError>;   // MTLCreateSystemDefaultDevice() → caps.check()
    pub(crate) fn raw(&self) -> &ProtocolObject<dyn MTLDevice>;
    pub(crate) fn allocate(&mut self, resource: Retained<ProtocolObject<dyn MTLResource>>) -> GpuResourceHandle;
    pub(crate) fn release(&mut self, handle: GpuResourceHandle) -> bool;   // 陈旧句柄 → false
    pub(crate) fn drain_retire(&mut self, completed: FrameIndex);
}
```

**D8 检测顺序（逐条，失败即 `UnsupportedGpu` / `AppInit`）**：

| # | 检测 | 失败 |
|---|---|---|
| 1 | `MTLCreateSystemDefaultDevice()` 返回 `Some`（`None` = 无 Metal 设备，Intel Mac 之外不会发生） | `UnsupportedGpu { os: "macos", actual: "no Metal device" }` |
| 2 | `argumentBuffersSupport() == MTLArgumentBuffersTier::Tier2`（编译验证过的常量名；`Tier1` 的机器会被拒） | `UnsupportedGpu { actual: "ArgumentBuffers Tier 1" }` |
| 3 | `CAMetalLayer::new()` 可用 + `setDevice(Some(device))` 后 `layer.device() == Some(device)`（`Swapchain::new` 内复核） | `AppInit`（图层不可用 ⇒ 窗口无法出帧） |
| 4 | `maxBufferLength >= 实例缓冲上限（初始 256 × 128 B = 32 KiB）` | `UnsupportedGpu`（理论不可达，只作防御） |

**device lost**：Metal 没有 `GetDeviceRemovedReason`。判据是 `MTLCommandBuffer.status == Error`，`reason` 取
`error.localizedDescription()` + `MTLCommandBufferError` 的 `code`（`objc2-metal` 有 `MTLCommandBufferError::DeviceRemoved = 11`）。
`log_path` = `std::env::temp_dir().join("kytrix-metal-<pid>-<frame>.txt")`，写入
`buffer.label` / `error.code` / `error.domain` / `error.userInfo` 的键列表（写失败只降级为路径本身）。
**不重建** device（主设计 §2.9）；`lost` 粘性，后续 `submit` 直接返回同一 `EngineError::DeviceLost`。

### 5.2 `macos/queue.rs`

```rust
pub(crate) struct Queue {
    raw: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    ring: InFlightRing,                 // 复用共享层的三槽记账（不写第二份）
    completed: FrameIndex,
}
impl Queue {
    pub(crate) fn new(device: &Device) -> Result<Self, EngineError>;
    pub(crate) fn begin(&mut self, device: &mut Device, index: FrameIndex) -> Result<(FrameSlot, Retained<ProtocolObject<dyn MTLCommandBuffer>>), EngineError>;
    /// commit → waitUntilCompleted → status/error 分类 → `advance(slot)` → `device.drain_retire(index)`
    pub(crate) fn submit(&mut self, device: &mut Device, slot: FrameSlot, buffer: &ProtocolObject<dyn MTLCommandBuffer>) -> Result<(), EngineError>;
    pub(crate) fn wait_idle(&mut self);
}
```


- 命令缓冲：`queue.commandBuffer()` 每帧一块（**不**跨帧复用；Metal 的命令缓冲是短命对象）。
- 提交：`commit()` → `waitUntilCompleted()` → 读 `status()`（`Completed` / `Error` / `NotEnqueued`）。
  同步等待与 D3D12 的 `Signal` + `WaitForSingleObject` 等价（结论 #6）。
- **不使用 `MTLSharedEvent`**：`MTLCommandBuffer::encodeWaitForEvent_value` 需要 `&ProtocolObject<dyn MTLEvent>`，
  而 `newSharedEvent()` 给的是 `ProtocolObject<dyn MTLSharedEvent>`，objc2 0.6.5 的 `ProtocolObject` **没有**协议上转
  （probe：`E0308`，只有 `from_ref` / `from_id` / `from_retained`）。v1 同步提交不需要它；P13 异步化时二选一：
  `msg_send![buffer, encodeWaitForEvent: &*event, value: v]`（带 SAFETY）或升级 objc2（F-120）。
- 每帧结束 `completed = index` → `device.drain_retire(index)`（满足主设计 §2.7 的「先确认已提交再释放」）。

### 5.3 `macos/swapchain.rs`

```rust
pub(crate) struct Swapchain {
    layer: Retained<CAMetalLayer>,
    format: MTLPixelFormat,        // BGRA8Unorm_sRGB（唯一取值）
    size: (u32, u32),              // 物理像素
    fresh: bool,
}
impl Swapchain {
    pub(crate) fn new(device: &Device, size: (u32, u32), scale: f64) -> Result<Self, EngineError>;
    pub(crate) fn layer(&self) -> &CAMetalLayer;                  // 交给 view.makeBackingLayer / setLayer
    pub(crate) fn next_drawable(&self) -> Option<Retained<ProtocolObject<dyn CAMetalDrawable>>>;
    pub(crate) fn resize(&mut self, size: (u32, u32), scale: f64);
    pub(crate) fn is_fresh(&self) -> bool;
}
```

**`CAMetalLayer` 参数（定死）**：

| 属性 | 值 | 理由 |
|---|---|---|
| `pixelFormat` | `MTLPixelFormat::BGRA8Unorm_sRGB` | 与 P07 契约的唯一编码点一致（MSL 输出 linear，由格式编码一次） |
| `opaque` | `true` | 引擎自己画满整面（无窗口透明需求；P09 的 `clear_color` 默认仍是不透明主题色） |
| `framebufferOnly` | **`false`** | 需要把 persist **blit 进** drawable 纹理；`true` 只保证「render target 用法」 |
| `maximumDrawableCount` | `3` | 与 `MAX_FRAMES_IN_FLIGHT` 对齐 |
| `allowsNextDrawableTimeout` | `false` | 超时返回 `None` 会让本帧空转；宁可阻塞（同步提交下由 present 节流） |
| `displaySyncEnabled` | `true` | 由 CoreAnimation 做 vsync 节流（等价 D3D12 的 `sync_interval = 1`） |
| `drawableSize` | `(size.0 × scale, size.1 × scale)` | **物理像素**；`contentsScale` 不参与渲染尺寸 |
| `presentsWithTransaction` | 默认 `false` | 渲染与图层提交解耦（性能） |

**无局部 present（定死）**：`nextDrawable()` 的 3 张纹理由系统轮转，**跨帧内容无保证** ⇒ 每帧必须写满整面：
`persist`（完整帧权威）→ `blit` 整面 → `presentDrawable`。damage 只在 pass 1 的 `setScissorRect` 上省 draw。

`nextDrawable() == None`（drawable 池被占满）⇒ **本帧跳过**（不录 GPU 工作、不推进 `FrameIndex` 的 GPU 部分，
但 `FrameIndex` 已递增）→ 返回 `Ok(())` 并记 `Section::Present` 为 0；**不**报错（不是失败）。

### 5.4 `macos/pipeline.rs`

```rust
pub(crate) struct ShaderSet {
    quad: Retained<ProtocolObject<dyn MTLLibrary>>,        // quad.metallib: vs_main / ps_main
    geometry: Retained<ProtocolObject<dyn MTLLibrary>>,    // geometry.metallib: vs_ui / vs_textured / ps_ui / ps_textured_image / ps_textured_glyph
}
impl ShaderSet {
    /// include_bytes!(OUT_DIR/shaders/*.metallib) → 占位检测 → newLibraryWithData:error:
    pub(crate) fn load(device: &Device) -> Result<Self, EngineError>;
}

pub(crate) struct Pipeline {
    quad: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    geometry_ui: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    geometry_textured_image: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    geometry_textured_glyph: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
    format: MTLPixelFormat,
}
impl Pipeline {
    pub(crate) fn new(device: &Device, shaders: &ShaderSet, format: MTLPixelFormat) -> Result<Self, EngineError>;
    pub(crate) fn quad(&self) -> &ProtocolObject<dyn MTLRenderPipelineState>;
    pub(crate) fn geometry(&self, pipeline: GeometryPipeline) -> &ProtocolObject<dyn MTLRenderPipelineState>;
    pub(crate) fn sampler(&self) -> &ProtocolObject<dyn MTLSamplerState>;
}
```

**加载（逐字）**：

```text
let bytes: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shaders/quad.metallib"));
if bytes.starts_with(b"KYTRIX-SHADER-SKIPPED") {
    return Err(EngineError::ShaderLoad { target: "metallib", path: OUT_DIR 的该文件路径 });
}
let data = DispatchData::from_bytes(bytes);
device.newLibraryWithData_error(&data)      // dispatch2（D-2）
      .map_err(|_| EngineError::ShaderLoad { target: "metallib", path })
```

- 入口名缺失（`newFunctionWithName` 返回 `None`）⇒ `EngineError::ShaderLoad`（同一 `target`，`path` 指向 metallib）。
- **PSO 状态（定死，与 P07 §2.9 / contract §7 逐字）**：

| 项 | 值 |
|---|---|
| quad VS/FS | `vs_main` / `ps_main`；**无** vertex descriptor（`vertex_id` + `instance_id`） |
| geometry VS/FS | `vs_ui`+`ps_ui`、`vs_textured`+`ps_textured_image`、`vs_textured`+`ps_textured_glyph`（三份 PSO） |
| geometry vertex descriptor | `attribute(0)`: `Float2` offset 0 buffer 0；`attribute(1)`: `Float2` offset 8 buffer 0；`layout(0).stride = 16`，`stepFunction = PerVertex`，`stepRate = 1`（`Ui` 用 stride 8 的变体：只声明 attribute 0 且 stride 8） |
| 颜色附件 | `pixelFormat = BGRA8Unorm_sRGB`；`blendingEnabled = true`；`sourceRGB = One`、`destinationRGB = OneMinusSourceAlpha`、`rgbBlendOperation = Add`；alpha 同上；`writeMask = All` |
| 深度/模板 | 无（`depthAttachmentPixelFormat` 不设置） |
| 采样 | `minFilter/magFilter = Linear`、`sAddressMode/tAddressMode/rAddressMode = ClampToEdge`、`mipFilter = NotMipmapped`、`lodMaxClamp = 0` |
| 拓扑 | `MTLPrimitiveType::TriangleStrip` + 4 顶点（geometry 按 `GeometryDesc.topology` 选 `TriangleList`/`TriangleStrip`） |
| cull | **不设置**（Metal 默认 `MTLCullMode::None`） |

### 5.5 `macos/atlas.rs`

```rust
pub(crate) struct AtlasTexture {
    texture: Retained<ProtocolObject<dyn MTLTexture>>,   // 2048² Private
    staging: Retained<ProtocolObject<dyn MTLBuffer>>,    // 页宽 × 256 行 × texel（StorageModeShared）
    page: u32,
    texel_bytes: u32,
    generation: u32,
}
pub(crate) struct AtlasTextures { pub glyph: AtlasTexture, pub image: AtlasTexture }
```

| 项 | 值 |
|---|---|
| glyph | `R8Unorm`，**非** sRGB（P07 §2.9）——`texture2d<float>` 采样 `.r` |
| image | `BGRA8Unorm_sRGB`（纹素是 linear 预乘后 sRGB 编码，P02 §10.3） |
| 存储 | `MTLStorageMode::Private` + `usage = ShaderRead | RenderTarget`（image 页需要被 render 目标写入？**不需要**，只 `ShaderRead`；保留 `ShaderRead` 即可） |
| 脏行上传 | `DirtyRows::spans()` 每段：写 staging → `copyFromBuffer:sourceOffset:sourceBytesPerRow:sourceBytesPerImage:sourceSize:toTexture:…`（blit） |
| 整页重传 | `texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(region, 0, NonNull<*mut c_void>, row_bytes)`（`generation` 变化时；probe 已验证签名） |
| staging 行 pitch | `page × texel_bytes`（Metal 的 blit `sourceBytesPerRow` 无 256 B 对齐要求，与 D3D12 不同） |
| generation | 与 `AtlasAllocator::generation` 对齐；变化 ⇒ 旧纹理 / staging 走 `retire`，本帧全量重传（P05 §10.6） |

### 5.6 `macos/compositor.rs` — `Renderer`（唯一 unsafe 汇聚点）

```rust
pub(crate) struct Renderer {
    device: Device,
    queue: Queue,
    pipeline: Pipeline,
    swapchain: Option<Swapchain>,     // None = 纯离屏（快照）
    persist: Persist,                 // 持久 offscreen（BGRA8Unorm_sRGB / Private）
    atlas: AtlasTextures,
    geometry: GeometryRegistry,       // 与 Windows 同形的 pub(crate) 最小实现
    upload: UploadArena,              // 每槽 2 块 buffer：instances(128 B 步进) + clips(48 B 步进)
    timer: Option<GpuTimer>,
    clear_color: [f32; 4],
    surface: (u32, u32),
    fresh: bool,
    lost: Option<EngineError>,
}
impl Renderer {
    pub(crate) fn new_offscreen(size: (u32, u32)) -> Result<Self, EngineError>;
    /// `view` 是 `NSView*` 的裸指针（与 Windows 的 `new_for_hwnd(isize)` 同形：`render/` 不 import `objc2_app_kit` 类型）。
    pub(crate) fn new_for_view(view: *mut c_void, size: (u32, u32), scale: f64) -> Result<Self, EngineError>;
    pub(crate) fn surface(&self) -> (u32, u32);
    pub(crate) fn is_fresh(&self) -> bool;
    pub(crate) fn resize(&mut self, size: (u32, u32)) -> Result<(), EngineError>;
    pub(crate) fn submit(&mut self, index: FrameIndex, buffer: &InstanceBuffer, plan: FramePlan, timings: &mut FrameTimings) -> Result<(), EngineError>;
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn readback(&mut self) -> Result<ReadbackImage, EngineError>;
    pub(crate) fn set_clear_color(&mut self, color: [f32; 4]);
    pub(crate) fn queue_atlas_upload(&mut self, kind: AtlasKind, pixels: &[u8], rows: DirtyRows);
    pub(crate) fn register_geometry(&mut self, id: GeometryId, desc: &GeometryDesc) -> Result<(), EngineError>;
}
```

`submit` 逐步（与 P08 §6.2 的 1:1 对照）：

```text
submit(index, buffer, plan, timings):
  1. if let Some(lost) = &self.lost { return Err(lost.clone()) }        // 粘性
  2. let (slot, cb) = queue.begin(device, index)?                       // ring.acquire
  3. surface 变化 → queue.wait_idle() → swapchain.resize(surface, scale)
                     → persist = Persist::new(...)（旧 persist 走 retire）→ fresh = true
  4. 上传（每槽两块 buffer，只写 [0, len*bytes)）
       instances: 128 B 步进；clips: 48 B（表项 0 是哨兵，CPU 值照抄）
  5. pass 1（persist = 唯一绘制目标）
       load = if plan.is_full() { Clear } else { Load }；store = Store
       setRenderPipelineState(quad)；setVertexBuffer(instances, 0, 0)；
       setFragmentBuffer(clips, 0, 1)；
       for rect in plan.plan().rects() { setScissorRect(rect) + drawPrimitives(TriangleStrip, 0, 4, len) }
       （geometry 段：setRenderPipelineState(geometry(..)) + setVertexBuffer(vbo, 0, 0) + drawIndexed）
  6. GPU 计时：cb 的 GPUStartTime/GPUEndTime 在 waitUntilCompleted 之后读（§5.8）
  7. pass 2：blitCommandEncoder → copyFromTexture(persist → drawable.texture) 整面 → endEncoding
  8. queue.submit(device, slot, cb)?          // commit → waitUntilCompleted → status/error → advance → drain_retire
  9. presentDrawable(drawable) 在 commit **之前**编码（第 7 步内），present 的「成功后清 fresh」由第 8 步完成
 10. timings：Upload（CPU）/ Gpu（GPUStart..End）/ Present（CPU）
```

**没有 barrier / 没有资源状态机**：Metal 自动跟踪 hazard（persist 在 `Submit` 边界被 command buffer 排序）。

### 5.7 `macos/readback.rs`

```rust
pub(crate) struct ReadbackImage { pub width: u32, pub height: u32, pub rgba8: Vec<u8> }
pub(crate) struct Readback { staging: Option<Retained<ProtocolObject<dyn MTLBuffer>>>, capacity: usize }
impl Readback {
    pub(crate) const fn new() -> Self;
    /// persist(Private) → blit 到 Shared buffer → 逐行去 padding → BGRA→RGBA
    pub(crate) fn capture(&mut self, device: &Device, queue: &Queue, texture: &ProtocolObject<dyn MTLTexture>, size: (u32, u32)) -> Result<ReadbackImage, EngineError>;
}
```

| 项 | 值 |
|---|---|
| 行 pitch | `width × 4`（Metal blit 无对齐要求；读取时按 `destinationBytesPerRow` = `width*4`） |
| 字节语义 | persist 是 `BGRA8Unorm_sRGB` ⇒ blit 出来的字节是 **sRGB 编码后的预乘值**（与 D3D12 的 `clear(0.5)→188` 同一契约） |
| 交换 | `chunks_exact_mut(4)` 里 `swap(0, 2)`（BGRA→RGBA），A 不动 |
| 时机 | 立即新建一块 command buffer 做 blit + `waitUntilCompleted`（同步；快照是测试路径） |

### 5.8 `macos/timer.rs`

```rust
pub(crate) struct GpuTimer;        // 无状态：Metal 的每帧 GPU 时间来自 command buffer 自身
impl GpuTimer {
    pub(crate) fn new(device: &Device) -> Result<Option<Self>, EngineError>;   // 恒 Ok(Some)
    pub(crate) fn resolve(&self, cb: &ProtocolObject<dyn MTLCommandBuffer>) -> Option<GpuTimerSample>;
}
```

- `gpu_ns = (cb.GPUStartTime() - cb.GPUEndTime()).abs() × 1e9`（秒 → ns，`f64` → `u64`）；
  `GPUStartTime > 0` 时才产出样本，否则 `None`（该帧没有 `Section::Gpu`）。
- **不用 `MTLCounterSampleBuffer`**：`device.counterSets()` 可能为空、采样点要拆 pass，且 CI 无 Xcode 无法验证；
  `GPUStartTime/GPUEndTime` 是「command buffer 级」的官方计时，足以喂 §2.10 的 `Section::Gpu`。
  `MTLCaptureManager`（Xcode GPU Frame Capture）只在调试构建按需接，不进 v1 主链路（诊断见 §9.3）。

### 5.9 Metal 帧流程（与 Win32 的对照）

| 阶段 | D3D12（P08） | Metal（P12） |
|---|---|---|
| 同步 | `ExecuteCommandLists` → `Signal` → `SetEventOnCompletion` + `WaitForSingleObject` | `commit()` → `waitUntilCompleted()` |
| 绘制目标 | persist（`B8G8R8A8_UNORM` + SRGB RTV 视图） | persist（`BGRA8Unorm_sRGB`） |
| 局部绘制 | `RSSetScissorRects(damage)` | `setScissorRect(damage)` |
| 到 present 目标 | 逐 damage `CopyTextureRegion` + `Present1(dirty)` | **整面** `blit` + `presentDrawable` |
| 前帧残留 | flip 模型保留；`fresh` 时全量 | **无保证** ⇒ 每帧整面写满 |
| GPU 计时 | timestamp query heap | `GPUStartTime` / `GPUEndTime` |
| 失败分类 | `GetDeviceRemovedReason` + DRED 落盘 | `status == Error` + `error` 落盘 |

### 5.10 argument buffer 上限与图集分批（事实 + 取舍）

| 事实 | 值 / 结论 |
|---|---|
| v1 的 MSL 绑定（P07 已落地，**唯一权威**） | `device const GpuPrimitive* instances [[buffer(0)]]`、`device const ClipEntry* clips [[buffer(1)]]`、`constant RootUniform& root [[buffer(2)]]`、`texture2d<float> glyph_atlas [[texture(0)]]`、`texture2d<float> image_atlas [[texture(1)]]`、`sampler atlas_sampler [[sampler(0)]]` |
| 因此 v1 **不需要** argument buffer | 无纹理数组、无间接访问；`contract.md` §7 的绑定表就是全部 |
| argument buffer 的真实上限 | Tier 2 的**纹理**槽上限远大于 2（Apple 的 feature set 表以 family 给值；引擎只需要 2 个）；**没有 sampler array** 是 Tier 1/2 的共同限制 ⇒ 若将来把图集拆成 N 页，必须**按页拆 draw**（`params1.y = texture_index` 已是现成的分批键），而**不是**塞进一个 argument buffer 的数组 |
| D8 为何仍检测 Tier 2 | 主设计 §2.2 的 macOS 硬基线；也是「将来 ≥3 页图集」的前置能力。v1 的判定不依赖它做绑定 |
| 图集分批（未来，不在 P12） | `FrameOp` 按 `texture_index` 分组 → 每组一次 `setFragmentTexture` + 一组 draw；与 D3D12 的 descriptor 槽位、Vulkan 的 binding 5/4 是同一策略 |

**⇒ 定案：v1 不引入 argument buffer**（否则要改 14 个 shader 源文件 + `contract.md` + `build.rs` 的锚点，并在
D3D12/Vulkan 上无对应物，违反 I8「三套语义等价」）。F-119 记录该决定与未来的分批路线。

---
## 6. Linux · Vulkan / Wayland 后端规格

> 只走 Wayland（D25 / I16）。`wayland-client 0.31.15` 的默认 feature 是纯 Rust 协议实现，但 **Vulkan WSI 不行**：
> `VkWaylandSurfaceCreateInfoKHR` 要 `wl_display*` / `wl_surface*` 真实对象，纯 Rust 后端的 `ObjectId` 只是令牌
> ⇒ 必须开 `features = ["system"]`（链 `libwayland-client`，`cargo tree` 出现 `wayland-sys`，F-135）。
> `xkbcommon 0.9` 只有 `#[link(name = "xkbcommon")]`（`cargo check` 不链接，链接期需要系统库）。
> 本节每个 ash / wayland 调用都已在 probe 里编译过（附录 B）。

### 6.1 `linux/instance.rs`

```rust
pub(crate) struct Instance {
    entry: ash::Entry,
    raw: ash::Instance,
    surface: vk::SurfaceKHR,
    surface_fn: ash::khr::surface::Instance,
    wayland_fn: ash::khr::wayland_surface::Instance,
}
impl Instance {
    /// `display` / `surface` 是 **`wl_display*` / `wl_surface*`**（调用方保证存活到 `Instance::drop`）。
    pub(crate) fn new(display: *mut c_void, surface: *mut c_void) -> Result<Self, EngineError>;
    pub(crate) fn raw(&self) -> &ash::Instance;
    pub(crate) fn surface(&self) -> vk::SurfaceKHR;
    pub(crate) fn extensions(&self) -> Vec<*const c_char>;    // 供 device 侧选 VK_KHR_swapchain / incremental_present
}
```

| 项 | 定死值 |
|---|---|
| `Entry` | `unsafe { ash::Entry::load() }`（`loaded` feature；失败 ⇒ `AppInit { detail: "libvulkan.so.1 …" }`） |
| `apiVersion` | `vk::make_api_version(0, 1, 2, 0)`（Vulkan 1.2 是硬基线；驱动更高版本自动向下兼容） |
| 实例扩展 | `VK_KHR_surface` + **`VK_KHR_wayland_surface`**（缺第二个 ⇒ `UnsupportedGpu { actual: "VK_KHR_wayland_surface missing" }`） |
| `pApplicationName` | `c"kytrix"`（0.38 的 builder 收 `&CStr`） |
| validation layer | 仅 `cfg(debug_assertions)` 且 `VK_LAYER_KHRONOS_validation` 在 `enumerate_instance_layer_properties` 里出现过才启用；**发布物永不启用** |
| surface | `wayland_fn.create_wayland_surface(&WaylandSurfaceCreateInfoKHR::default().display(display).surface(surface))`（`wl_display*`/`wl_surface*` 来自平台层，I9：不出 `platform/`） |

### 6.2 `linux/device.rs`

```rust
pub(crate) struct GpuCaps {
    pub adapter: SmolStr,                 // VkPhysicalDeviceProperties.deviceName
    pub api_version: u32,                 // 原始 u32，`actual()` 里格式化成 "1.2.281"
    pub device_type: vk::PhysicalDeviceType,
    pub descriptor_indexing: bool,
    pub runtime_descriptor_array: bool,   // v1 未使用，仅记录
    pub timeline_semaphore: bool,
    pub swapchain: bool,                  // VK_KHR_swapchain 在设备扩展表里
    pub wayland_surface: bool,            // VK_KHR_wayland_surface（实例扩展）
    pub incremental_present: bool,        // VK_KHR_incremental_present（仅 hint，§6.3）
}
impl GpuCaps {
    pub(crate) fn baseline_ok(&self) -> bool;      // api>=1.2 && descriptor_indexing && timeline && swapchain && wayland_surface
    pub(crate) fn actual(&self) -> SmolStr;        // "Vulkan 1.1.0 / descriptorIndexing=false / …"
    pub(crate) fn describe(&self) -> SmolStr;
    pub(crate) fn check(&self) -> Result<(), EngineError>;   // UnsupportedGpu { os: "linux", .. }
}

pub(crate) struct Device {
    physical: vk::PhysicalDevice,
    raw: ash::Device,
    queue: vk::Queue,
    queue_family: u32,
    swapchain_fn: ash::khr::swapchain::Device,
    timeline: vk::Semaphore,
    fences: [vk::Fence; MAX_FRAMES_IN_FLIGHT],
    inflight: InFlightRing,
    caps: GpuCaps,
    resources: ResourceTable,      // GpuResourceHandle → GpuObject（image/buffer/view/pipeline/sampler …）
    lost: Option<EngineError>,
}
```

**选卡与 D8 采集（逐条）**：

| # | 检测 | 失败 |
|---|---|---|
| 1 | `enumerate_physical_devices` 中取第一个满足 `get_physical_device_surface_support(pd, family, surface) == true` 的设备 | `UnsupportedGpu`（无可用 GPU / 无 Wayland surface 支持） |
| 2 | `properties.api_version >= VK_API_VERSION_1_2` | `UnsupportedGpu { required: "Vulkan 1.2" }` |
| 3 | `PhysicalDeviceVulkan12Features{ timeline_semaphore, descriptor_indexing, ... }`（`get_physical_device_features2`） | 缺 `timeline_semaphore` ⇒ 硬失败；**`descriptor_indexing` 按 D-4 裁决**（默认硬失败，与主设计 §2.2 的 required 文案一致） |
| 4 | 设备扩展表含 `VK_KHR_swapchain`（实例扩展含 `VK_KHR_wayland_surface`） | `UnsupportedGpu` |
| 5 | 图形 + present 队列族（v1 优先取同一个族；不同族需 `sharing_mode(Concurrent)`，本机未验证 ⇒ v1 要求同族，否则 `UnsupportedGpu`） | `UnsupportedGpu` |
| 6 | `VK_KHR_incremental_present` 只在扩展表里出现时设置 `caps.incremental_present`（**仅 hint**） | 不失败 |

**创建 device（定死）**：`enabled_extension_names = [VK_KHR_swapchain (+ VK_KHR_incremental_present 若存在)]`，
`pNext = PhysicalDeviceVulkan12Features { timeline_semaphore: true, descriptor_indexing: true, runtime_descriptor_array: true }`
（`runtime_descriptor_array` 只为将来的图集分批准备，v1 不用 ⇒ 若驱动不支持则**降级为 false** 并继续，不做硬失败）。

**同步（定死）**：

```text
timeline semaphore（1 个，initial_value 0）：每次 submit signal `FrameIndex::get()`
fence（每 in-flight 槽 1 个）：submit 时 signal，wait_for_fences 后 reset（同步提交）
retire：submit 返回后 `timeline_value = index` ⇒ `device.drain_retire(index)`（无需再等，与 D3D12 同序）
device lost：任何 `VkResult::ERROR_DEVICE_LOST` ⇒ `EngineError::DeviceLost`（粘性停止提交）
诊断：debug 构建 + 有 validation 时把 layer 输出写 `%TEMP%/kytrix-vk-<pid>-<frame>.txt`（`log_path`）
```

### 6.3 `linux/swapchain.rs`

```rust
pub(crate) struct Swapchain {
    raw: vk::SwapchainKHR,
    fn_: ash::khr::swapchain::Device,
    images: Vec<vk::Image>,
    format: vk::Format,           // B8G8R8A8_SRGB / R8G8B8A8_SRGB
    extent: vk::Extent2D,
    incremental: bool,            // VK_KHR_incremental_present 可用
    fresh: bool,
    pending_acquire: Option<(u32, vk::Semaphore)>,   // 本帧 acquire 结果
    render_finished: [vk::Semaphore; MAX_FRAMES_IN_FLIGHT],
}
impl Swapchain {
    pub(crate) fn new(instance: &Instance, device: &Device, size: (u32, u32)) -> Result<Self, EngineError>;
    pub(crate) fn acquire(&mut self, device: &Device, index: FrameIndex) -> Result<u32, EngineError>;  // suboptimal ⇒ fresh = true
    pub(crate) fn present(&mut self, device: &Device, frame: FramePlan, image_index: u32) -> Result<(), EngineError>;
    pub(crate) fn resize(&mut self, instance: &Instance, device: &Device, size: (u32, u32)) -> Result<(), EngineError>;
    pub(crate) fn format(&self) -> vk::Format;
    pub(crate) fn extent(&self) -> vk::Extent2D;
}
```

| 项 | 定死值 |
|---|---|
| 格式 | 从 `get_physical_device_surface_formats` 里挑 `B8G8R8A8_SRGB`，否则 `R8G8B8A8_SRGB`，否则 `UnsupportedGpu`（`actual: "no *_SRGB surface format"`）。**必须 SRGB**：persist 与 swapchain 同格式才能 `cmd_copy_image`，且 sRGB 编码只发生一次 |
| colorspace | `VK_COLOR_SPACE_SRGB_NONLINEAR_KHR`（与上面成对，取同一 `SurfaceFormatKHR`） |
| present mode | 优先 `FIFO`（等价 vsync）；`MAILBOX` **不启用**（省电与可预测性优先，且 CI 无实机可验） |
| image count | `min_image_count = clamp(caps.min_image_count + 1, 3, caps.max_image_count)` |
| usage | `COLOR_ATTACHMENT | TRANSFER_DST`（persist 用 `cmd_copy_image` 写它） |
| composite alpha | 优先 `OPAQUE`（`supported_composite_alpha` 里挑） |
| preTransform | 取 `caps.current_transform`（给 `IDENTITY` 之外的平台也正确） |
| clipped | `true` |
| 局部 present | `caps.incremental_present && matches!(frame.present_dirty(), Some(_))` ⇒ 在 `PresentInfoKHR::pNext` 挂 `PresentRegionsKHR { swapchain_count: 1, p_regions: [PresentRegionKHR { rectangles: [RectLayerKHR { offset, extent, layer: 0 }] }] }`；**失败/被忽略都不影响正确性**（§0 #10） |

**`acquire_next_image` 的三种结果**：`Ok((i, false))` 正常；`Ok((i, true))` suboptimal ⇒ `fresh = true` 且本帧仍提交；
`ERROR_OUT_OF_DATE_KHR` ⇒ `resize()` 后**跳过本帧**（`Ok(())`，不报错）；`ERROR_SURFACE_LOST_KHR` ⇒
`EngineError::Swapchain { detail }`（不重试、不降级）。

### 6.4 `linux/pipeline.rs`

```rust
pub(crate) struct Pipeline {
    descriptor_layout: vk::DescriptorSetLayout,   // 4 个 binding
    set: vk::DescriptorSet,                       // 唯一一份（v1 单 in-flight GPU 资源，见下）
    pool: vk::DescriptorPool,
    layout: vk::PipelineLayout,                   // set 0 + push constants [0,112)
    quad: vk::Pipeline,
    geometry_ui: vk::Pipeline,
    geometry_textured_image: vk::Pipeline,
    geometry_textured_glyph: vk::Pipeline,
    shaders: ShaderSet,                           // 7 个 ShaderModule（include_bytes! SPIR-V）
    sampler: vk::Sampler,
}
impl Pipeline {
    pub(crate) fn new(device: &Device, format: vk::Format) -> Result<Self, EngineError>;
    /// 每槽写入 instances/clips 的 descriptor（v1 用 3 份 set，与 MAX_FRAMES_IN_FLIGHT 对齐）
    pub(crate) fn write_slot(&mut self, device: &Device, slot: usize, instances: vk::Buffer, clips: vk::Buffer);
    pub(crate) fn write_atlas(&mut self, device: &Device, kind: AtlasKind, view: vk::ImageView);
}
```

| 项 | 定死值（与 P07 `common.glsl` 逐字对应） |
|---|---|
| descriptor set 0 | `binding 0` = STORAGE_BUFFER（instances，`ShaderStageFlags::VERTEX`）；`binding 1` = STORAGE_BUFFER（clips，`FRAGMENT`）；`binding 4` = COMBINED_IMAGE_SAMPLER（glyph）；`binding 5` = COMBINED_IMAGE_SAMPLER（image）。binding 2/3 空缺（GLSL 不用，Vulkan 允许稀疏） |
| push constants | `VK_SHADER_STAGE_VERTEX_BIT | FRAGMENT_BIT`，`offset 0`，`size 112`（`RootUniform`；Vulkan 保证 ≥128 B） |
| descriptor 池 | `max_sets = MAX_FRAMES_IN_FLIGHT`，`STORAGE_BUFFER × 2×3`、`COMBINED_IMAGE_SAMPLER × 2×3`；**不回收**（`GpuResourceHandle` 的 `kind = 2` 编码为将来动态 SRV 预留，P12 不产生） |
| shader module | `build.rs` 的 7 个 `.spv`（`include_bytes!`）→ `create_shader_module`；magic `0x07230203` 由 `build.rs` 校验，加载器再查一次首 4 字节 = `03 02 23 07`；占位串 `KYTRIX-SHADER-SKIPPED` ⇒ `EngineError::ShaderLoad { target: "spirv", path }` |
| 入口名 | 恒 `main`（`c"main"`） |
| 顶点输入 | quad：无（vertex/instance index）；geometry `Ui`：`attribute 0 = float2` stride 8；`Textured`：`attribute 0 = float2` offset 0 + `attribute 1 = float2` offset 8，stride 16 |
| 光栅 | `polygonMode FILL`、`cullMode NONE`、`frontFace COUNTER_CLOCKWISE`、`lineWidth 1.0` |
| 混合 | `blendEnable = true`；color：`ONE` / `ONE_MINUS_SRC_ALPHA` / `ADD`；alpha：`ONE` / `ONE_MINUS_SRC_ALPHA` / `ADD`；`colorWriteMask RGBA` |
| 深度/模板 | `PipelineDepthStencilStateCreateInfo::default()`（无附件 ⇒ 必须与 `renderPass = null` + `dynamic rendering` 配合） |
| 拓扑 | quad `TRIANGLE_STRIP`；geometry 按 `GeometryDesc.topology`（`TRIANGLE_LIST` / `TRIANGLE_STRIP`）——两条 pipeline 各自缓存一份 |
| dynamic state | `VIEWPORT` + `SCISSOR`（damage 矩形每帧变） |
| 渲染通道 | **核心 `VkRenderPass` + `VkFramebuffer`**（persist 一个附件，`load = CLEAR/LOAD`、`store = STORE`）；不用 `VK_KHR_dynamic_rendering`（1.2 下是扩展，F-126） |

**为什么最终选 `VkRenderPass`**：dynamic rendering 在 1.2 是扩展（`VK_KHR_dynamic_rendering`，1.3 才核心）；Wayland 桌面驱动普遍有，
但「普遍」不是基线。v1 的 target 是 1.2 ⇒ 用核心 `VkRenderPass`（1 个附件 = persist；swapchain image 不建 framebuffer，因为只用
`cmd_copy_image` 写它）。这是 F-126。

### 6.5 `linux/atlas.rs`

```rust
pub(crate) struct AtlasTexture {
    image: vk::Image,          // OPTIMAL tiling
    view: vk::ImageView,       // 与 image 同格式（glyph R8_UNORM；image B8G8R8A8_SRGB）
    sampler: vk::Sampler,
    staging: vk::Buffer,       // HOST_VISIBLE | HOST_COHERENT，容量 = page² × texel
    staging_memory: vk::DeviceMemory,
    generation: u32,
}
pub(crate) struct AtlasTextures { pub glyph: AtlasTexture, pub image: AtlasTexture }
```

| 项 | 值 |
|---|---|
| glyph | `VK_FORMAT_R8_UNORM`，`usage = SAMPLED | TRANSFER_DST`，`initialLayout = UNDEFINED` |
| image | `VK_FORMAT_B8G8R8A8_SRGB`（**必须 SRGB**：P07 §8；sRGB 解码由格式完成，shader 不再手写） |
| 上传 | `cmd_pipeline_barrier(旧: SHADER_READ_ONLY → TRANSFER_DST)` → `cmd_copy_buffer_to_image`（`bufferRowLength = page`、`bufferImageHeight = rows`、`imageExtent = (page, rows)`）→ barrier 回 `SHADER_READ_ONLY_OPTIMAL` |
| 行 pitch | `page × texel`（Vulkan 的 `bufferRowLength` 是纹素数，不是字节；**无** 256 B 对齐要求） |
| 整页重传 | `generation` 变化 ⇒ 旧 image/view/sampler/staging 走 `retire`；新 image 从 `UNDEFINED` 一次 `cmd_copy_buffer_to_image` 全页 |
| 内存 | `find_memory_type` 按 `HOST_VISIBLE | HOST_COHERENT` 选型（**不**用 `vkMapMemory` 持久映射，避免与 `VK_EXT_external_memory` 的将来冲突） |

### 6.6 `linux/compositor.rs` — `Renderer` + barrier 表

```rust
pub(crate) struct Renderer {
    instance: Instance,
    device: Device,
    swapchain: Option<Swapchain>,
    pipeline: Pipeline,
    persist: Persist,                 // 与 swapchain 同格式的 image + view
    atlas: AtlasTextures,
    geometry: GeometryRegistry,
    upload: UploadArena,              // 每槽 instances/clips 的 HOST_VISIBLE buffer
    command_pool: vk::CommandPool,
    commands: [vk::CommandBuffer; MAX_FRAMES_IN_FLIGHT],
    timer: Option<GpuTimer>,
    clear_color: [f32; 4],
    surface: (u32, u32),
    fresh: bool,
    lost: Option<EngineError>,
}
```

`submit` 逐步：

```text
submit(index, buffer, plan, timings):
  0. lost 早退
  1. queue.wait_for_fences(&[fences[slot]], true) → reset_fences                                 // 同步提交
  2. surface 变化 → device_wait_idle() → swapchain.resize(...) + persist.recreate(...) → fresh = true
  3. acquire_next_image(index) → image_index（out-of-date ⇒ resize 后 return Ok(())）
  4. 上传：instances/clips 写 HOST_VISIBLE buffer（`[0, len*bytes)`；容量不够 ⇒ next_power_of_two 重建 + retire）
  5. begin_command_buffer(BEGIN_ONE_TIME_SUBMIT)
  6. persist：UNDEFINED/PRESENT? → COLOR_ATTACHMENT_OPTIMAL（`oldLayout = COLOR_ATTACHMENT_OPTIMAL`，首帧 UNDEFINED）
       begin_render_pass(load = if full { CLEAR } else { LOAD }，store = STORE)
       cmd_bind_pipeline(quad) + cmd_set_viewport + 逐 damage：cmd_set_scissor + cmd_draw(4, len, 0, 0)
       geometry：cmd_bind_vertex_buffers + cmd_bind_index_buffer + cmd_draw_indexed
       cmd_end_render_pass
  7. persist：COLOR_ATTACHMENT_OPTIMAL → TRANSFER_SRC_OPTIMAL
  8. swapchain image：UNDEFINED → TRANSFER_DST_OPTIMAL
  9. cmd_copy_image(persist 整面 → swapchain image 整面)        // 每帧整面（§0 #4）
 10. swapchain image：TRANSFER_DST_OPTIMAL → PRESENT_SRC_KHR
 11. timer：cmd_reset_query_pool + write_timestamp(TOP_OF_PIPE, 0) … write_timestamp(BOTTOM_OF_PIPE, 1)
 12. end_command_buffer → queue_submit(wait: acquire_semaphore(STAGE_COLOR_ATTACHMENT_OUTPUT), signal: render_finished[slot] + timeline=index)
 13. wait_for_fences(fences[slot])（signal 的 fence）
 14. queue_present(wait: render_finished[slot], image_index, pNext = present_regions hint)
 15. drain_retire(index)；timings：Upload / Gpu（query pool）/ Present
```

**barrier 表（每帧固定，无动态状态机）**：

| 资源 | before → after | src/dst stage | 时机 |
|---|---|---|---|
| persist | `COLOR_ATTACHMENT_OPTIMAL` → `TRANSFER_SRC_OPTIMAL` | `COLOR_ATTACHMENT_OUTPUT` → `TRANSFER` | draw 之后 |
| persist | `TRANSFER_SRC_OPTIMAL` → `COLOR_ATTACHMENT_OPTIMAL` | `TRANSFER` → `COLOR_ATTACHMENT_OUTPUT` | copy 之后（下一帧绘制前） |
| swapchain image | `UNDEFINED` → `TRANSFER_DST_OPTIMAL` | `TOP_OF_PIPE` → `TRANSFER` | copy 之前 |
| swapchain image | `TRANSFER_DST_OPTIMAL` → `PRESENT_SRC_KHR` | `TRANSFER` → `BOTTOM_OF_PIPE` | copy 之后 |
| 图集纹理 | `SHADER_READ_ONLY` → `TRANSFER_DST` → `SHADER_READ_ONLY` | 见 §6.5 | 仅有脏行时 |
| 上传 buffer | 无（HOST_COHERENT，fence 保证不重叠） | — | — |

### 6.7 `linux/readback.rs`

```rust
pub(crate) struct ReadbackImage { pub width: u32, pub height: u32, pub rgba8: Vec<u8> }
pub(crate) struct Readback { buffer: vk::Buffer, memory: vk::DeviceMemory, capacity: u64 }
impl Readback {
    pub(crate) const fn new() -> Self;
    pub(crate) fn capture(&mut self, device: &Device, command: vk::CommandBuffer, image: vk::Image, size: (u32, u32)) -> Result<ReadbackImage, EngineError>;
}
```

| 项 | 值 |
|---|---|
| buffer | `TRANSFER_DST`，`HOST_VISIBLE | HOST_COHERENT` 内存；容量 = `w*h*4`，超出则重建 |
| 拷贝 | `cmd_copy_image_to_buffer`（`imageSubresource` 单层，`bufferImageHeight`/`bufferRowLength` 均 0 = tightly packed） |
| 读 | `map_memory` → `copy_nonoverlapping` → `unmap_memory` → `chunks_exact_mut(4).swap(0, 2)`（BGRA→RGBA） |
| 语义 | persist 是 `*_SRGB` ⇒ 读到的是 sRGB 编码后的预乘字节（与 D3D12 / Metal 同契约） |

### 6.8 `linux/timer.rs`

```rust
pub(crate) struct GpuTimer { pool: vk::QueryPool, period_ns: f64 }   // 2 × MAX_FRAMES_IN_FLIGHT 个 query
impl GpuTimer {
    pub(crate) fn new(device: &Device, timestamp_period: f32) -> Result<Option<Self>, EngineError>;
    pub(crate) fn begin(&self, device: &Device, command: vk::CommandBuffer, slot: usize);
    pub(crate) fn end(&self, device: &Device, command: vk::CommandBuffer, slot: usize);
    pub(crate) fn resolve(&self, device: &Device, slot: usize, ticks_per_ns: f64) -> Option<u64>;
}
```

- `queue_family_properties.timestamp_valid_bits == 0` ⇒ `GpuTimer::new` 返回 `Ok(None)`（**不是错误**：计时是可选能力）。
- `timestampPeriod`（ns/tick，`f32`）来自 `VkPhysicalDeviceLimits`；`gpu_ns = (end - start) * period`。
- `get_query_pool_results(WAIT | TYPE_64)` 在 fence 之后读；失败 ⇒ `None`（不 panic）。

### 6.9 Wayland WSI 与 present 语义（定死）

| 项 | 结论 |
|---|---|
| 谁 commit `wl_surface` | **Vulkan WSI**（`vkQueuePresentKHR` 内部 attach/commit）；平台层**不得**再对同一 surface `commit()`（会与 WSI 抢 buffer） |
| swapchain image 内容 | present 之后**未定义** ⇒ 每帧整面 copy（§0 #4） |
| 局部 present | 只有 `VK_KHR_incremental_present` 的 hint，且**只**在 `present_dirty()` 为 `Some` 时挂；`fresh` 帧不挂 |
| 帧节流 | `FIFO` present mode（vsync）；`wl_surface.frame` 回调**不**用于渲染节流（WSI 自己管），只用于「空闲时不提交」的门禁 |
| resize | `xdg_toplevel.configure` → 平台层 `Window::set_size`（逻辑）→ `commit_frame` 里 `surface != extent` ⇒ `resize` |
| `wl_buffer` 释放 | WSI 内部管理（引擎不碰 `wl_buffer`） |

---
## 7. 平台层 · macOS（`platform/macos/**`）

### 7.1 `macos/event_loop.rs` — 引擎自有循环（`NSApplication` + CFRunLoop）

```rust
pub(crate) struct MacLoop {
    mtm: MainThreadMarker,
    app: Retained<NSApplication>,
    window: NativeWindow,
    wake: WakeSource,                 // CFRunLoopSource（主 run loop）
    timer: Option<CFRetained<CFRunLoopTimer>>,
    state: Box<LoopState>,            // 泵状态（dispatcher / 事件队列 / 帧门禁）
}
impl MacLoop {
    pub(crate) fn run(app: &mut App) -> Result<(), EngineError>;   // Application::run 的实现
}
```

**逐字生命周期**：

```text
Application::run(app):
 1. main_thread_only: MainThreadMarker::new()（非主线程 ⇒ AppInit）
 2. NSApplication::sharedApplication(mtm) → setActivationPolicy(Regular)
 3. for (id, window) in app.windows():
      a. NativeWindow::create(mtm, title, logical_size)          // NSWindow + GuiView（隐藏：不 makeKeyAndOrderFront）
      b. scale = window.backingScaleFactor() → Scale::new(scale as f32).ok_or(EngineError::InvalidDpiScale { factor })?
      c. Renderer::new_for_view(view_ptr, physical(size, scale), scale)?   // Device::new → caps.check()
      d. Window::set_renderer(renderer)；window.set_scale(scale)
      e. a11y adapter（§7.7）**必须在窗口首次显示之前**构造
 4. window.show()（makeKeyAndOrderFront）
 5. WakeSource::new()（CFRunLoop::main + CFRunLoopSource，回调 = pump）
 6. AppHandle::wake()（首帧）
 7. **实现更正（F-133）**：不调 `NSApplication::run()`——`Application::run` 自己跑 `nextEventMatchingMask:untilDate:` 的 drain/阻塞 + `CFRunLoopSource` 唤醒 + 动画定时器，理由见 DEVIATIONS §17.4 F-133（原设计写的是 `app.run()` + 在 run 里不写循环）
 8. run 返回（stop:）⇒ shutdown：Renderer::queue.wait_idle() → drop renderer（retire 全排空）
      → a11y drop（deactivate 由平台 adapter 自理）→ window close → 返回 Ok(())
```

**帧泵（`WakeSource::perform` 回调，逐字）**：

```text
pump(state):
  1. drain 平台事件：for event in state.events.drain(..) { app.dispatch(window, event) }
  2. dispatcher.take_frame_wake() ⇒ frame_all(&mut app)：
       for (id, _) in app.windows() { app.run_frame_hook(); app.refresh_styles(id); app.frame(id)? }
  3. for task in dispatcher.drain_tasks() { task(&mut app) }
  4. 若 app.windows() 为空 ⇒ NSApplication::stop(None)
  5. 若 app.has_animations() ⇒ 确保 CFRunLoopTimer 已装（间隔 = 下一 tick 的剩余时间，最小 1 ms）；
     否则 invalidate 定时器（**空闲零帧 + 零唤醒**，I3）
```

**唤醒（任意线程，`AppHandle::wake` / `Dispatcher::push`）**：

```text
dispatcher.wake():
  if !frame_pending.swap(true, AcqRel) { wake_source.signal(); wake_source.wake_up(); }
```

- `CFRunLoopSourceSignal` / `CFRunLoopWakeUp` 都是线程安全的（CoreFoundation 文档）；`CFRunLoopSource` 以
  `order = 0` 装到 main run loop 的 **default mode**（`add_source(source, None)`）。
- **不用 libdispatch**：不引 `dispatch_async`（`dispatch2` 只在 `render/macos/pipeline.rs` 里用于 `DispatchData`，D-2）。
- **不子类化 `NSApplication`**：帧泵挂在 CFRunLoopSource 上。**实现更正（F-133）**：因为要自己持有主循环
  （`nextEventMatchingMask:untilDate:` 的 drain + `distantFuture` 阻塞），所以不调 `NSApplication::run()`；
  ObjC 运行时风险面仍然只有 `GuiView` 一个子类，与 Win32「泵 + 唤醒」的职责划分一致。
- 定时器：`CFRunLoopTimer::new(fire_date, interval, flags: 0, order: 0, callout, context)`（无 `repeats` 参数：
  `interval > 0` 即自动重排）；**实现更正（F-133）**：定时器**按需**创建/移除（有动画才挂），callout 里跑的是同一个泵。

### 7.2 `macos/view.rs` — `NSView` 子类 + `CAMetalLayer` + `NSTextInputClient`

**ObjC 类定义（编译验证过的形状）**：

```rust
#[derive(Debug)]
pub(crate) struct ViewIvars {
    scale: Cell<f64>,
    events: RefCell<Vec<Event>>,          // 平台事件（泵 drain）
    ime: RefCell<ImeState>,               // §7.5
    a11y: RefCell<Option<MacA11y>>,       // §7.7（adapter 必须晚于 view 存在，故 Option）
    layer: RefCell<Option<Retained<CAMetalLayer>>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "KytrixGuiView"]
    #[ivars = ViewIvars]
    pub(crate) struct GuiView;

    impl GuiView {                       // 覆盖 NSView 的方法（**不是** trait impl）
        #[unsafe(method(isFlipped))]     fn is_flipped(&self) -> bool { true }
        #[unsafe(method(acceptsFirstResponder))] fn accepts_first_responder(&self) -> bool { true }
        #[unsafe(method(acceptsFirstMouse:))] fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool { true }
        #[unsafe(method_id(makeBackingLayer))] fn make_backing_layer(&self) -> Retained<CALayer> { /* CAMetalLayer */ }
        #[unsafe(method(viewDidChangeBackingProperties))] fn backing_properties_changed(&self) { /* 重新读 backingScaleFactor */ }
        #[unsafe(method(keyDown:))]      fn key_down(&self, e: &NSEvent) { /* 先 IME，再 Key/Text */ }
        #[unsafe(method(flagsChanged:))] fn flags_changed(&self, e: &NSEvent) { /* 修饰键位图 */ }
        #[unsafe(method(mouseDown:))]    fn mouse_down(&self, e: &NSEvent) { … }
        #[unsafe(method(mouseMoved:))]   fn mouse_moved(&self, e: &NSEvent) { … }
        #[unsafe(method(scrollWheel:))]  fn scroll_wheel(&self, e: &NSEvent) { … }
        #[unsafe(method(isAccessibilityElement))] fn is_accessibility_element(&self) -> bool { false }
        #[unsafe(method_id(accessibilityChildren))] fn accessibility_children(&self) -> Option<Retained<NSArray>> { … }
        #[unsafe(method_id(accessibilityHitTest:))] fn accessibility_hit_test(&self, point: NSPoint) -> Option<Retained<AnyObject>> { … }
        #[unsafe(method_id(accessibilityFocusedUIElement))] fn accessibility_focused_ui_element(&self) -> Retained<AnyObject> { … }
    }

    unsafe impl NSTextInputClient for GuiView { /* 9 个必需方法，见 §7.5 */ }
);
```

**objc2 0.6.5 的三条硬规则（probe 实测）**：

1. 覆盖父类方法写在 `impl GuiView { #[unsafe(method(...))] }`；**不能**写 `unsafe impl NSView`（`NSView` 是 struct 不是 trait ⇒ `E0404`）。
2. **返回对象的方法必须用 `#[unsafe(method_id(...))]`**：`makeBackingLayer` / `accessibilityChildren` /
   `accessibilityHitTest:` / `accessibilityFocusedUIElement` 用 `method` 会得到 `Encode`/`EncodeReturn` 的 `E0277`（probe 实测）。
3. `unsafe impl NSTextInputClient` 会**编译期检查必需方法齐全**（objc2 文档：漏写或多写都报错）⇒ 9 个必需方法一次写全。

**图层绑定（定死）**：

```text
view.setWantsLayer(true)
view.setLayer(Some(&metal_layer))            // 安全方法（probe：无需 unsafe）
metal_layer.setContentsScale(backingScaleFactor)
metal_layer.setDrawableSize(physical size)
```

`makeBackingLayer` 与 `setLayer` 二者只做一份：**用 `makeBackingLayer`**（AppKit 在 `wantsLayer` 转 true 时调用一次），
`layer` 由 ivars 缓存，`Renderer` 通过 `view.layer()` 拿到它。`CAMetalLayer` 的其余参数见 §5.3。

**事件收集**：`GuiView` 只把事件 push 进 `ViewIvars.events`（`RefCell<Vec<Event>>`），**不碰 `App`**
（与 Win32 `WndProc` 同一纪律：平台回调里没有 `&mut App`）。泵在 `WakeSource` 回调里 drain 并 dispatch。

### 7.3 `macos/window.rs`

| 项 | 定死值 |
|---|---|
| 创建 | `NSWindow::initWithContentRect_styleMask_backing_defer(alloc, rect, style, Buffered, false)` |
| styleMask | `Titled | Closable | Miniaturizable | Resizable | FullSizeContentView`（自绘标题栏：内容延伸到标题栏区域） |
| 标题栏 | `setTitlebarAppearsTransparent(true)`、`setTitleVisibility(Hidden)`；`setOpaque(false)`、`setBackgroundColor(clearColor)` |
| 交通灯 | `standardWindowButton(CloseButton)` **保留**并重排到 `x = 12`（逻辑点）；`MiniaturizeButton` / `ZoomButton` 隐藏。**隐藏后必须提供等价的引擎绘制控件**（P13 的窗口控件），否则用户失去最小化/最大化（F-127） |
| 拖拽 | `performWindowDragWithEvent(event)`（`NSEvent` 的左键拖动落在标题栏区域时；引擎自绘标题栏的 `WindowControlArea` 语义在 P13 接） |
| 最小尺寸 | `setContentMinSize(NSSize(320, 200))`（`MIN_SIZE_LOGICAL` 与 Win32 同源） |
| 尺寸回读 | `contentView.frame.size`（点，逻辑像素）→ `Window::set_size` |
| 关闭 | `NSWindowWillCloseNotification`（`NSNotificationCenter`，**不用 `NSWindowDelegate`** —— P01 未声明该 feature，且通知足够）；收到后 `App::close_window(id)` |
| 缩放/焦点 | `NSWindowDidChangeBackingPropertiesNotification`（scale）/ `NSWindowDidResizeNotification` / `NSWindowDidBecomeKeyNotification` / `NSWindowDidResignKeyNotification` |

> `NSWindowDelegate` / `NSApplicationDelegate` 的 trait 在 objc2-app-kit 0.3.2 里**随 `NSWindow` / `NSApplication` feature 提供**
> （probe 实测），但 P12 刻意走通知而不是 delegate：`App` 的生命周期由泵掌握，delegate 会引入第二个回调入口。

### 7.4 `macos/dpi.rs`

```rust
pub(crate) fn scale_from(backing_scale_factor: f64) -> Result<Scale, EngineError>;   // Scale::new(f32) → None ⇒ InvalidDpiScale { factor }
pub(crate) fn physical(size: Size, scale: Scale) -> (u32, u32);                       // covering，至少 1 px
```

- 事件：`viewDidChangeBackingProperties` / `NSWindowDidChangeBackingPropertiesNotification` ⇒
  `Event::DpiChanged { dpi: (scale*96).round() as u32, scale }`（`dpi` 只为与 Win32 的字段对齐；引擎只读 `scale`）。
- 缩放变化 ⇒ `Window::set_scale`（根 `LAYOUT` + offscreen 重建 + 文本 measure 版本递增），**VBO 与实例容量不重建**（§2.9）。

### 7.5 `macos/ime.rs` — `NSTextInputClient` 最小集

**9 个必需方法 + 语义（定死）**：

| 方法 | 实现 |
|---|---|
| `insertText:replacementRange:` | `AnyObject` → `NSString`（`msg_send![text, description]` 或 `isKindOfClass(NSString)` 后取）⇒ `ImeEvent::Commit` + 清 `marked` |
| `setMarkedText:selectedRange:replacementRange:` | 存 `marked` + 选中范围 ⇒ `ImeEvent::Preedit` |
| `unmarkText` | 清 `marked` ⇒ `ImeEvent::Preedit(None)`（引擎侧用空串表达） |
| `hasMarkedText` | `!marked.is_empty()` |
| `markedRange` | 有组合 ⇒ `NSRange(0, marked.utf16_len)`；否则 `NSRange(NSUInteger::MAX, 0)` |
| `selectedRange` | 未实现选区 ⇒ `NSRange(NSUInteger::MAX, 0)`（P13 的文本选区接上后给真实值） |
| `attributedSubstringForProposedRange:actualRange:` | `None`（引擎不让 IME 读回富文本） |
| `validAttributesForMarkedText` | 空 `NSArray`（无富文本属性） |
| `firstRectForCharacterRange:actualRange:` | 屏幕坐标的**光标矩形**（`NSRect`；P16 由 `App::set_ime_cursor_area` → `Window::text_caret_rect` 提供；无 hint ⇒ 窗口原点 1×1） |
| `characterIndexForPoint:` | `0`（无字符索引模型） |
| `doCommandBySelector:` | 吞掉（`sel` 记录到诊断日志；不转发给 `keyDown`） |

**事件序**：`keyDown:` 先 `inputContext().handleEvent(event)`（IME 优先），若 `hasMarkedText()` 或本次事件被 IME 消费
则**不再**发 `Event::Input(Key/Text)`；否则 Key 走 `InputEvent::Key`，可打印字符走 `InputEvent::Text`。
**dead key**：AppKit 的 `NSEvent.characters` 已给出组合结果（`flagsChanged` 不产生字符），引擎不做 `TIS` 层处理（V-84：L5 验）。

**中文/日文输入**：`marked` 用 `Vec<u16>` 存 UTF-16（`NSRange` 是 UTF-16 单位），提交时一次性转 `SmolStr`。

### 7.6 `macos/clipboard.rs`

```rust
pub(crate) fn set_text(text: &str) -> Result<(), EngineError>;   // NSPasteboard::generalPasteboard + clearContents + setString:forType:
pub(crate) fn get_text() -> Result<SmolStr, EngineError>;        // stringForType:（NSPasteboardTypeString）
```

`NSPasteboardTypeString` 是 extern static ⇒ **`unsafe { NSPasteboardTypeString }`**（probe 实测：安全上下文里访问会 `E0133`）。
`setString:forType:` 是安全方法；返回 `bool`（`false` ⇒ `EngineError::Clipboard`）。

### 7.7 `macos/a11y.rs`

```rust
pub(crate) struct MacA11y {
    adapter: accesskit_macos::Adapter,
    activation: MacActivation,           // request_initial_tree → None（placeholder 语义）
}
impl MacA11y {
    /// 必须在主线程 + 窗口首次显示之前调用；`view` 转 `*mut c_void`（adapter 内部 retain）。
    pub(crate) fn new(view: &GuiView, dispatcher: Arc<Dispatcher>, window: WindowId) -> Result<Self, EngineError>;
    pub(crate) fn update(&mut self, factory: impl FnOnce() -> TreeUpdate);     // update_if_active → raise()
    pub(crate) fn focus_changed(&mut self, focused: bool);                     // update_view_focus_state → raise()
    pub(crate) fn hit_test(&mut self, x: f64, y: f64) -> Option<Retained<AnyObject>>;
    pub(crate) fn children(&mut self) -> Option<Retained<NSArray>>;
    pub(crate) fn focused(&mut self) -> Option<Retained<AnyObject>>;
}
```

**probe 验证过的事实（全部影响实现）**：

| # | 事实 | 影响 |
|---|---|---|
| 1 | `Adapter::new(view: *mut c_void, is_focused: bool, action_handler)` 是 **`unsafe`**，且内部 `MainThreadMarker::new().unwrap()` | 非主线程构造会 **panic** ⇒ `new` 只能在 `Application::run` 里；`EngineError::A11yInit` 只覆盖句柄无效 |
| 2 | `update_if_active(f) -> Option<QueuedEvents>`，`QueuedEvents::raise()` | `raise()` 在**借用释放之后**调用（与 P11 的 Windows 纪律同构：raise 期间可能重入 NSAccessibility） |
| 3 | `hit_test(point: accesskit_macos::NSPoint, &mut H) -> *mut NSObject` | 点是 **objc2 0.2 线**的 `NSPoint`（`accesskit_macos::NSPoint { x, y }` 直接构造）；返回**裸指针**（autoreleased）⇒ `unsafe { Retained::retain(raw.cast::<AnyObject>()) }` |
| 4 | `view_children(&mut H) -> *mut NSArray<NSObject>`（0.2 线） | `raw.cast::<NSArray>()` 后 `Retained::retain` |
| 5 | `focus(&mut H) -> *mut NSObject` | 空指针 ⇒ 返回 `nil`（AppKit 语义：无焦点元素） |
| 6 | 三者都可能 **初始化 placeholder context**（`get_or_init_context`） | 首次 `accessibilityHitTest:` 早于首帧也安全（会先用 placeholder 树） |
| 7 | **没有** `register_hit_test`（P11 附录 A.3 N15①） | 命中由 view 的 `accessibilityHitTest:` 直接调 adapter |

**`GuiView` 的 NSAccessibility 回调（逐字）**：

```text
isAccessibilityElement            → false（容器）
accessibilityChildren             → adapter.view_children() → Retained<NSArray>
accessibilityHitTest:(NSPoint)    → adapter.hit_test(point) → Retained<AnyObject>
accessibilityFocusedUIElement     → adapter.focus() → 非空则返回，否则 self
```

`update` 的调用点：`Window::update_a11y`（P11 已落）在泵的每轮检查 `app.a11y_pending(window)`，
macOS 复用同一入口（`NativeWindow::update_a11y` 与 Windows 同名同形）。

### 7.8 `NSEvent → Event` 映射（定死）

| `NSEvent` / 来源 | 产出 |
|---|---|
| `mouseMoved` / `mouseDragged` / `rightMouseDragged` / `otherMouseDragged` | `Input(PointerMove { position: logical, modifiers })` |
| `leftMouseDown` / `rightMouseDown` / `otherMouseDown` | `Input(PointerDown { button: Primary/Secondary/Middle/X1/X2, .. })` |
| `leftMouseUp` / … | `Input(PointerUp { .. })` |
| `mouseExited`（`NSTrackingArea` 的 `MouseExited`） | `Input(PointerLeave)` |
| `scrollWheel` | `Input(Scroll { delta_x, delta_y, .. })`：`hasPreciseScrollingDeltas == true` ⇒ 像素/行因子 1.0；否则 `scrollingDeltaY / 3.0`（**行**，一档 ≈ 3 点；与 Win32 的 120 单位/行不同源，只能靠 L5 校准，V-85） |
| `keyDown` / `keyUp` | `Input(Key { key: map_keycode(event.keyCode()), state, modifiers })` |
| `flagsChanged` | 只更新 `Modifiers` 位图（**不**发 `Key`：与 Win32 一致，修饰键靠 `Key` 之外的位图） |
| `keyDown` 且产生字符（非 IME 消费） | `Input(Text { text: SmolStr })` |
| `viewDidChangeBackingProperties` | `DpiChanged { dpi, scale }` |
| `NSWindowDidResizeNotification` | `Resized { size: logical }` |
| `NSWindowDidBecomeKeyNotification` / `…ResignKey…` | `FocusChanged { focused }` |
| `NSWindowWillCloseNotification` | `CloseRequested` |
| `NSApplicationDidChangeEffectiveAppearanceNotification`（或 `NSApp.effectiveAppearance`） | `AppearanceChanged { dark }` |

**坐标**：`NSEvent.locationInWindow` 是**窗口逻辑点**（AppKit 已按 `backingScaleFactor` 归一）；view 是 `isFlipped = true`
（左上原点），所以 `convertPoint:fromView:nil` 后直接用；**不做**额外除法。
**键码表**：`kVK_*`（`NSEvent.keyCode`，硬件码）→ `Key::{Escape, Enter, Tab, Backspace, Delete, Space, Arrow*, Home, End, PageUp/Down, Function(n), Shift/Control/Alt/Meta}`；
未映射 ⇒ `Key::Other(kVK 值 as u16)`（与 Win32 的 `Key::Other(VK)` 同一约定）。
**修饰键**：`NSEventModifierFlags::{Shift, Control, Option, Command}` → `Modifiers::{SHIFT, CONTROL, ALT, META}`（`Option` = Alt，`Command` = Meta）。

---

## 8. 平台层 · Linux（`platform/linux/**`）

### 8.1 `linux/event_loop.rs` — `calloop` + `wayland-client`

```rust
pub(crate) struct LinuxLoop {
    loop_: EventLoop<'static, LoopState>,
    ping: calloop::ping::Ping,
    connection: Connection,
    queue: EventQueue<WaylandState>,
    globals: GlobalList,
    window: NativeWindow,
}
impl LinuxLoop {
    pub(crate) fn run(app: &mut App) -> Result<(), EngineError>;
}
```

**逐字启动时序**：

```text
Application::run(app):
 1. Connection::connect_to_env()                 // 无 WAYLAND_DISPLAY ⇒ WaylandCompositor { detail }
 2. registry_queue_init::<WaylandState>(&conn)   // 失败 ⇒ WaylandCompositor
 3. globals.bind: wl_compositor(4..=6) / xdg_wm_base(1..=6) / wl_seat(5..=9) / wl_data_device_manager(3..=3)
      任一缺失或版本不足 ⇒ WaylandCompositor { detail: "… not advertised" }（I16：不回退 X11）
 4. wl_surface + xdg_surface + xdg_toplevel（§8.2）；首帧前 surface.commit()
 5. roundtrip 一次，拿到首个 configure（尺寸 + states）与分数缩放
 6. Renderer::new_for_surface(display_ptr, surface_ptr, physical(size, scale), scale)?
       // Instance::new → Device::new → Swapchain::new → Pipeline/Atlas/Persist
 7. a11y adapter（§8.7）**在窗口首次提交前**构造
 8. EventLoop::try_new() + calloop::ping::make_ping() + WaylandSource::new(conn, queue).insert(handle)
 9. AppHandle::wake() ⇒ 首帧；loop_.dispatch(Duration, &mut state) 直到 exit
10. shutdown：device_wait_idle → drop swapchain/device/instance → a11y drop
```

**泵（每次 `dispatch` 前后各跑一次 pump）**：

```text
loop {
  drain 平台事件 → app.dispatch(window, event)
  if dispatcher.take_frame_wake() { frame_all(&mut app)? }
  for task in dispatcher.drain_tasks() { task(&mut app) }
  if exit_requested || app.windows().is_empty() { break }
  loop_.dispatch(timeout, &mut state)?          // timeout = 有动画 ? 下一 tick : None（阻塞，I3）
}
```

- **唤醒**：`calloop::ping::Ping`（`Arc` 持有，任意线程 `ping()`）；`dispatch` 被唤醒后先 drain（顺序与 Win32 的
  「先 drain 后阻塞」一致）。
- **`WaylandSource`** 负责把 `EventQueue` 的可读事件接进 calloop；`dispatch_pending` 由它内部调用。
- **不轮询**：无动画 / 无待处理帧时 `dispatch(None, ..)` 永久阻塞；`wl_surface.frame` 回调只用来「不要连发提交」。

### 8.2 `linux/surface.rs` — `xdg_toplevel` / 分数缩放 / 装饰

```rust
pub(crate) struct NativeSurface {
    compositor: wl_compositor::WlCompositor,
    surface: wl_surface::WlSurface,
    xdg_surface: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    fractional: Option<wp_fractional_scale_v1::WpFractionalScaleV1>,
    viewport: Option<wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport>,
    decoration: Option<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>,
    pending_configure: Option<u32>,        // serial
    size: Size,                            // 逻辑像素
    scale: Scale,
    closed: bool,
}
```

| 项 | 定死值 |
|---|---|
| `xdg_surface.ack_configure(serial)` | 每条 `xdg_surface::Event::Configure` **必须** ack（否则不再收到 configure） |
| `xdg_toplevel::Event::Configure { width, height, states }` | `width/height == 0` ⇒ 用当前逻辑尺寸；否则 `Window::set_size(logical)`。**不**在事件里直接改 surface 尺寸，等下一帧 `commit_frame` 的 resize |
| `xdg_toplevel::Event::Close` | `Event::CloseRequested` + `xdg_toplevel.destroy()` |
| `xdg_toplevel::Event::ConfigureBounds` | 记录（v1 不做约束裁剪） |
| `xdg_wm_base::Event::Ping(serial)` | **必须** `pong(serial)`（否则 compositor 判定无响应） |
| `set_title` / `set_app_id` | 窗口标题 / `"kytrix"`；`set_min_size(physical(MIN_SIZE_LOGICAL))` |
| 移动 / 缩放 | `xdg_toplevel::move(&seat, serial)` / `resize(&seat, serial, XdgToplevelResizeEdge)`（标题栏拖拽由引擎自绘区域触发） |
| 最大化 / 全屏 | `set_maximized()` / `unset_maximized()`（自绘标题栏的双击行为） |
| 分数缩放 | `wp_fractional_scale_manager_v1.get_fractional_scale(&surface)` → `Event::PreferredScale { scale }`（单位 **1/120**）⇒ `Scale::new(scale as f32 / 120.0)` |
| 整数缩放回退 | 无 fractional 扩展时用 `wl_surface::Event::Enter(output)` + `wl_output::Event::Scale(n)`（整数） |
| 缓冲尺寸 | `physical = (logical × scale).ceil()`（覆盖式取整，`Round::Covering`，与 Win32 同源） |
| viewporter | 仅当「`scale × 120` 不是 120 的整数倍」时创建 `wp_viewporter.get_viewport(&surface)` 并 `set_destination(logical)`（否则不建，避免多余协议对象） |
| 客户端装饰 | 有 `zxdg_decoration_manager_v1` ⇒ `get_toplevel_decoration(&toplevel).set_mode(ClientSide)`；**没有也不失败**（compositor 可能加自己的装饰；L5 记录，V-86） |
| `wl_surface.set_buffer_scale` | **不设置**（WSI + fractional scale 管；设了会与 viewporter 冲突） |

### 8.3 `linux/window.rs`

| 项 | 值 |
|---|---|
| 逻辑尺寸 | `Size`（引擎单位），物理 = `× scale` 的 covering |
| 最小尺寸 | `MIN_SIZE_LOGICAL = (320, 200)`（与 Win32 / macOS 同源），物理值给 `set_min_size` |
| 标题栏 | **自绘**（引擎绘制窗口控制区）；`toplevel.move/resize` 由平台层在标题栏拖动时调用 |
| 关闭 | `toplevel.destroy()` + `surface.destroy()`；`LinuxLoop` 见 `app.windows()` 空即退出 |
| 光标 | v1 **不设置** `wl_cursor`（无 `wayland-cursor` 依赖）；`take_cursor_change()` 的值只在 macOS/Win32 生效（V-87：Linux 光标形状 L5 补） |
| 键盘焦点 | `wl_keyboard::Event::Enter/Leave` ⇒ `FocusChanged { focused }` |

### 8.4 `linux/dpi.rs`

```rust
pub(crate) fn scale_from(preferred_scale: i32) -> Result<Scale, EngineError>;   // 120 ⇒ 1.0（Scale::new(v/120f32)，None ⇒ InvalidDpiScale）
```

- 分数缩放事件 ⇒ `Event::DpiChanged { dpi: (scale*96).round() as u32, scale }`（字段对齐见 §7.4）。
- 缩放变化 ⇒ offscreen/swapchain 重建 + 根 `LAYOUT` + 文本 measure 版本递增；**VBO/实例容量不重建**。
- `wl_output::Event::Scale` 只在**同一 output** 上变化时触发；多显示器跨 output 移动时以 `wl_surface::Event::Enter` 的
  output 为准（`preferred_scale` 由 compositor 按 surface 所在 output 给，是权威值 ⇒ 优先用它）。

### 8.5 `linux/ime.rs` — `zwp_text_input_v3` 状态机

```rust
pub(crate) struct ImeState {
    text_input: Option<ZwpTextInputV3>,
    enabled: bool,
    focused_surface: bool,
    pending: PendingState,          // 双缓冲：preedit/commit 只在 done 时生效
    last_commit: SmolStr,           // 去重（compositor 可能重复发）
}
```

**协议状态机（定死；事件序必须按 `done` 提交）**：

| 客户端请求 | 时机 |
|---|---|
| `enable()` + `set_content_type(hint, purpose)` + `set_cursor_rectangle(x, y, w, h)` + `set_surrounding_text(text, cursor, anchor)` + `set_text_change_cause(cause)` → **`commit()`** | `wl_keyboard::Enter` 且引擎有文本焦点时；光标/选区变化时再次成组发送 |
| `disable()` + `commit()` | `wl_keyboard::Leave` 或引擎失去文本焦点 |
| `destroy()` | 窗口销毁 |

| 服务端事件 | 处理 |
|---|---|
| `preedit_string(text, cursor_begin, cursor_end)` | 存入 `pending.preedit`（**不**立即生效） |
| `commit_string(text)` | 存入 `pending.commit` |
| `delete_surrounding_text(before_length, after_length)` | 存入 `pending.delete`（P13 的文本编辑模型接上后消费；v1 只记录） |
| `done(serial)` | **原子生效**：`commit` ⇒ `ImeEvent::Commit`；`preedit` 非空 ⇒ `Preedit`；空 ⇒ `End`；清 `pending` |

- **顺序陷阱**：`preedit_string` / `commit_string` 可能在同一批里出现（`done` 前）⇒ 必须缓存到 `done` 再发，
  否则 UI 会看到「先 preedit 再 commit」的半状态（F-128）。
- **去重**：`commit_string` 与随后到达的 `wl_keyboard::key`（compositor 不应发，但 GNOME 在部分版本会）——
  引擎在 `commit` 后吞掉同一帧内的字符键（用 `last_commit` + 时间窗 0，只吞同一 `Serial` 的紧随 key）。
- **候选窗定位**：v3 由 compositor 决定，引擎只提供 `set_cursor_rectangle`（逻辑像素 → compositor 期望**逻辑**坐标）。
- 多个 seat：v1 只接第一个 `wl_seat` 的 `get_text_input`（V-88）。

### 8.6 `linux/clipboard.rs` — `wl_data_device` / `wl_data_source`

```rust
pub(crate) struct Clipboard {
    device: wl_data_device::WlDataDevice,
    source: Option<wl_data_source::WlDataSource>,   // 我们发布的内容
    cached: Option<SmolStr>,                        // 最近一次收到的选区内容（同步 API 的唯一来源）
    serial: u32,
}
```

| 操作 | 协议步骤 |
|---|---|
| `set_text(text)` | `wl_data_device_manager.create_data_source()` → `offer("text/plain;charset=utf-8")` → `data_source::Event::Send { mime_type, fd }` 时把 `text` 写进 `fd`（`std::fs::File::from(fd)`）并 drop ⇒ 关闭写端 → `data_device.set_selection(Some(&source), serial)` |
| `get_text()` | **返回 `cached`**（同步 API）。缓存来源：`data_device::Event::DataOffer(offer)` → `offer.receive("text/plain;charset=utf-8", fd)`；随后 `data_device::Event::Selection(Some(offer))` 才视为有效；读端 fd 由事件循环在可读时读满 → 存 `cached`（`SmolStr`） |
| 失效 | `Selection(None)` ⇒ `cached = None`；`offer.destroy()` 后不再读 |
| 序列号 | `serial` 来自最近的输入事件（`wl_pointer::Event::Enter` / `wl_keyboard::Event::Enter` 的 serial）；没有 serial 时 `set_selection` 会被 compositor 忽略（**必须**用真实 serial，F-129） |

**读为什么要异步**：Wayland 的剪贴板是「拉」模型，`receive` 之后数据由对端写 pipe；主线程不能阻塞读（会冻泵）。
所以 `get_text()` 只能返回缓存；`App::clipboard_text()` 在 Linux 上返回 `Err(Clipboard { detail: "clipboard not yet received" })` 若从未收到过内容。
**这是与 Win32/macOS 的行为差异**（F-130），必须在 `App::clipboard_text` 的文档里写明。

### 8.7 `linux/a11y.rs` — `accesskit_unix`（AT-SPI over D-Bus）

```rust
pub(crate) struct UnixA11y { adapter: accesskit_unix::Adapter }
impl UnixA11y {
    /// `Adapter::new(activation, action, deactivation)`：三个 handler 都会被**库的线程**调用 ⇒ 全部要 `Send`。
    pub(crate) fn new(dispatcher: Arc<Dispatcher>, window: WindowId) -> Result<Self, EngineError>;
    pub(crate) fn update(&mut self, factory: impl FnOnce() -> TreeUpdate);   // 无返回值
    pub(crate) fn focus_changed(&mut self, focused: bool);                   // update_window_focus_state(bool)
    pub(crate) fn set_bounds(&mut self, outer: Rect, inner: Rect);           // Wayland 下 outer == inner
}
```

**probe / 源码验证过的事实**：

| # | 事实 | 影响 |
|---|---|---|
| 1 | `Adapter::new(activation, action, deactivation)` **安全函数**，三个 handler 都要求 `Send`，**在别的线程调用** | action handler 只能 `Dispatcher::push`（与 Windows 同纪律） |
| 2 | `request_initial_tree() → None` **合法**：`context.rs:91-109` 把状态置 `Pending`，**下一次 `update_if_active` 提供的更新必须是全树** | 复用 P11 的 `build_update`：`A11yBridge` 的**第一次** `take` 必然是全树（`full_pending = true`）⇒ 不新增 `full_update` 辅助 |
| 3 | `update_if_active(f)` **无返回值** | 不需要 `raise()`；AT-SPI 由库广播 |
| 4 | `update_window_focus_state(bool)` 存在（无返回值） | 窗口焦点走它 |
| 5 | `set_root_window_bounds(outer, inner)` 的文档说明：Wayland 拿不到窗口位置，「只在 X11 有意义」 | Linux 下 `outer = inner = Rect { 0, 0, w, h }`（逻辑像素）；**不**报错 |
| 6 | `DeactivationHandler::deactivate_accessibility()` 由**库自己**在 deactivate / `Drop`（`RemoveAdapter`）时调用 | 引擎只实现一个空 handler（清本地标志）；**不需要**手动 deactivate（与 P11 §12 的措辞不同，F-124） |
| 7 | `accesskit_unix 0.24.1` 内部自带线程 + `Executor`；`tokio` feature 只影响内部 channel 类型 | 不占引擎 runtime；`--features tokio` 来自 P01 |

### 8.8 `wl_* → Event` 映射（定死）

| 协议事件 | 产出 |
|---|---|
| `wl_pointer::Event::Enter { surface, surface_x, surface_y, .. }` | 记录 serial；`Input(PointerMove)` |
| `wl_pointer::Event::Motion { surface_x, surface_y }` | `Input(PointerMove { position: logical, .. })` |
| `wl_pointer::Event::Button { button, state, .. }` | `Button`：`0x110` = `Primary`、`0x111` = `Middle`、`0x112` = `Secondary`、`0x113/0x114` = `X1/X2` ⇒ `PointerDown/Up` |
| `wl_pointer::Event::Axis { axis, value }` | `Input(Scroll { delta_x/delta_y, .. })`：`value` 是 **120 分之一行**（Wayland 的 `axis` 单位 = 1/120 行）⇒ `delta = -value / 120.0`（Wayland 的 `value` 正方向 = 向下/向右 ⇒ 取反对齐 Win32 的「正 = 向上」） |
| `wl_pointer::Event::Frame` | 一帧输入结束（批量提交的边界；v1 不做合帧） |
| `wl_pointer::Event::Leave` | `Input(PointerLeave)` |
| `wl_keyboard::Event::Keymap { format: XkbV1, fd, size }` | `xkb::Keymap::new_from_fd(&ctx, fd, size, TextV1, NoFlags)`（`unsafe`）→ `xkb::State::new` |
| `wl_keyboard::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group }` | 缓存三元组；`Modifiers` 位图由 `xkb_state` 的 `MOD_NAME_SHIFT/CONTROL/ALT/LOGO` 求得 |
| `wl_keyboard::Event::Key { key, state, .. }` | `state.update_mask(...)` → `key_get_one_sym` / `key_get_utf8`；`Keysym` → `Key` 映射见下；有文本则再发 `Input(Text)` |
| `wl_keyboard::Event::Enter { serial, .. }` | 记录 serial（剪贴板 + IME 都要）→ `FocusChanged(true)` |
| `wl_keyboard::Event::Leave` | `FocusChanged(false)` |
| `wl_data_device::Event::DataOffer/Selection/Leave` | §8.6 |
| `wp_fractional_scale_v1::Event::PreferredScale { scale }` | `DpiChanged`（§8.4） |
| `xdg_surface::Event::Configure` | `ack_configure`；尺寸/缩放事件合并到下一帧 |
| `xdg_toplevel::Event::Close` | `CloseRequested` |
| `wl_callback::Event::Done` | 帧节流许可（允许下一次提交） |

**`Keysym → Key` 映射（定死，`xkb::Keysym` 常量）**：

| Keysym | `Key` |
|---|---|
| `Escape` / `Return` / `Tab` / `BackSpace` / `Delete` / `space` | `Escape` / `Enter` / `Tab` / `Backspace` / `Delete` / `Space` |
| `Up` / `Down` / `Left` / `Right` | `ArrowUp` / `ArrowDown` / `ArrowLeft` / `ArrowRight` |
| `Home` / `End` / `Prior` / `Next` | `Home` / `End` / `PageUp` / `PageDown` |
| `F1..F24` | `Function(n)` |
| `Shift_L/R` / `Control_L/R` / `Alt_L/R` / `Super_L/R` | `Shift` / `Control` / `Alt` / `Meta` |
| 其它（含字母/数字） | `Key::Other(evdev keycode)`（`wl_keyboard.key` 是 evdev 码 + 8 ⇒ `key - 8`） |

- **字符输入**：`State::key_get_utf8` 返回非空 ⇒ `InputEvent::Text`（与 Win32 的 `WM_CHAR` 同一契约）。
  `key_get_utf8` 在 Ctrl 组合下可能返回控制字符 ⇒ 只接受 `!c.is_control()`。
- **修饰键位图**：**不**从 `mods_depressed` 直接映射（NumLock/CapsLock 会进来）；走 `xkb_state_mod_name_is_active`
  的四个名字（Probe 未验证该 API ⇒ 实现期二选一：`serialize_mods` 位图 或 `mod_name_is_active`，见 V-89）。

---
## 9. 错误与诊断

### 9.1 错误产生点（**不新增变体**，全部复用 `error.rs`）

| 变体 | macOS 产生点 | Linux 产生点 |
|---|---|---|
| `UnsupportedGpu { os, required, actual }` | `Device::new`：无 Metal 设备 / `argumentBuffersSupport != Tier2` | `Instance::new`（缺 `VK_KHR_wayland_surface`）、`Device::new`（无合适物理设备 / API < 1.2 / `timeline_semaphore` / `descriptor_indexing` / 无同族 graphics+present / 无 `VK_KHR_swapchain` / 无 `*_SRGB` surface 格式） |
| `DeviceLost { reason, log_path }` | `MTLCommandBuffer.status == Error`（`MTLCommandBufferError::DeviceRemoved` 等）+ 诊断文件 | `VK_ERROR_DEVICE_LOST` + validation 输出文件 |
| `WaylandCompositor { detail }` | —（不适用） | 无 `WAYLAND_DISPLAY` / `Connection::connect_to_env` 失败 / 缺 `wl_compositor`、`xdg_wm_base`、`wl_seat`、`wl_data_device_manager` / 版本不足 |
| `ShaderLoad { target, path }` | `"metallib"`：占位串、`newLibraryWithData` 失败、入口名缺失 | `"spirv"`：占位串、magic 不符、`create_shader_module` 失败 |
| `Swapchain { detail }` | `nextDrawable` 之外的图层失败（`drawableSize == 0` 等）；`resize` 后纹理创建失败 | `create_swapchain_khr` / `resize` 失败 / `ERROR_SURFACE_LOST_KHR` |
| `AppInit { detail }` | `MainThreadMarker::new()` 失败、`Entry`/instance 级失败（Linux）、`CAMetalLayer` 不可用 | `ash::Entry::load` 失败、`create_instance` 失败、`create_device` 失败 |
| `WindowCreate { detail }` | `NSWindow` 创建返回 nil（理论不可达，作防御） | `xdg_surface` / `xdg_toplevel` 协议错误（`wl_display` 报错时） |
| `Clipboard { detail }` | `setString_forType` 返回 `false` | `set_selection` 无 serial / `receive` 失败 / 从未收到内容（§8.6） |
| `InvalidDpiScale { factor }` | `backingScaleFactor` 非有限或 ≤ 0 | `preferred_scale` 非正 / NaN |
| `A11yInit { detail }` | `Adapter::new` 前置检查失败（view 指针为空） | `Adapter::new` 无法连接 session bus（不阻塞：D-Bus 不在时会 `Inactive`，见下） |

> **D-Bus 缺失不是错误**：`accesskit_unix` 在无 session bus / AT-SPI 未启用时保持 `Inactive`，
> `update_if_active` 的闭包不执行 ⇒ 零开销（与 Windows 的懒激活同构）。`A11yInit` 只覆盖「构造参数非法」。

### 9.2 D8 检测项总表（三平台并列，实现即验收面）

| 平台 | 硬基线（缺即 `UnsupportedGpu` 退出） | 记录项（`describe()` 输出，不失败） |
|---|---|---|
| Windows（P08 已落） | FL ≥ 12_0、SM ≥ 6.2、BindingTier ≥ 2、FLIP_SEQUENTIAL + BufferCount ≥ 2 | heap tier、tiled、min precision、native16、root sig、sampler heap、warp |
| macOS（P12） | `MTLCreateSystemDefaultDevice` 非空、`argumentBuffersSupport == Tier2`、`CAMetalLayer` 可创建 | `registry_id`、`unified_memory`、`low_power`、`max_buffer_length`、`maxThreadsPerThreadgroup` |
| Linux（P12） | Vulkan ≥ 1.2、`timeline_semaphore`、`descriptor_indexing`（D-4）、`VK_KHR_swapchain`、`VK_KHR_wayland_surface`、同族 graphics+present、surface 有 `*_SRGB` 格式、compositor 有 `xdg_wm_base` | `device_type`、`runtime_descriptor_array`、`incremental_present`、`timestamp_valid_bits`、`maxImageDimension2D` |

### 9.3 诊断（P12 只接**已有**日志路径，不引 `log` crate）

| 项 | 做法 |
|---|---|
| `GpuCaps::describe()` | `EngineError::AppInit` 的 `detail` 里带上（启动失败时用户能看到实际值）与 `UnsupportedGpu.actual`（只含基线项） |
| Metal 错误 | `MTLCommandBufferError` 的 `code`/`domain` + `localizedDescription`；`MTLCaptureManager` **不**在 v1 接（V-90） |
| Vulkan 错误 | debug 构建 + validation 可用时，把 layer 回调收集的行写入 `%TEMP%/kytrix-vk-<pid>-<frame>.txt`；`DebugUtilsMessengerEXT` **不**在 v1 接（1.2 需 `VK_EXT_debug_utils`，非基线） |
| Wayland 错误 | `wl_display` 的 error 事件 → `WaylandCompositor { detail: "protocol error: …" }`；`xdg_wm_base.ping` 超时（v1 不设超时）记诊断 |
| 帧计时 | `FrameTimings` 的 `Section::{Upload, Gpu, Present}` 三段，三平台同名（P08 已定义） |

---

## 10. 静态审查清单（三平台最容易漂移的点）

> 本阶段没有 mac/Linux 运行时，**这张表就是验收面**：实现期逐条对照，review 逐条勾。

### 10.1 三套 shader 的语义对照（MSL / GLSL vs HLSL，逐条）

| # | 项 | HLSL（Windows） | MSL（macOS） | GLSL（Vulkan） | 检查方式 |
|---|---|---|---|---|---|
| 1 | NDC Y | `(x*2/w-1, 1-y*2/h)` | **同 HLSL**（`common.metal::to_ndc`：`1.0 - y * viewport.w`） | `(x*2/w-1, y*2/h-1)`（`common.glsl::to_ndc`） | 读 7 个 GLSL / 2 个 MSL 文件的 `to_ndc`；`contract.md` §5 |
| 2 | 纹理原点 | 左上、v 向下 | 同 | 同（**不**做 v 翻转） | 三份 `common.*` 的 UV 注释 + 图集上传（P02 §10.2） |
| 3 | sRGB | 目标格式 `*_UNORM_SRGB` | 目标格式 `.bgra8Unorm_srgb` | 目标格式 `*_SRGB` | PSO/管线创建的附件格式（§5.4 / §6.4）；glyph 图集必须**非** sRGB |
| 4 | 预乘 | 输入已是线性预乘；blend `ONE / ONE_MINUS_SRC_ALPHA` | 同 | 同 | 三处 blend state 常量（§5.4 表 / §6.4 表 / P08 §5.4） |
| 5 | gamma | `apply_gamma` 恒等 | 同 | 同 | `grep apply_gamma`（三份都是 `return c;`） |
| 6 | clip | `clip_index == 0` 早退；表项 0 不参与运算 | 同（`clip_mask(... , clips)`） | 同（`clip_mask(..., clips)` 从 `binding 1` 读） | 三份 `clip_mask` |
| 7 | 实例下标 | `nointerpolation uint` | `uint [[flat]]` | `flat out/in uint` | `quad.*` 的 VS 输出 |
| 8 | 实例读取 | `SV_InstanceID` → SRV `t0` | `instance_id` → `buffer(0)` | `gl_InstanceIndex` → `binding 0` | 三份 quad VS 的第一行 |
| 9 | 绑定表 | `t0/t1/t2/t3` + `b0`(28 DWORD) + 静态 `s0` | `buffer(0/1/2)` + `texture(0/1)` + `sampler(0)` | `set 0 binding 0/1/4/5` + push constants 112 B | `contract.md` §7 与三个后端的绑定代码**逐项对齐** |
| 10 | 拓扑 | `TriangleStrip` + 4 顶点 | 同 | 同 | 三处管线创建（`PrimitiveTopologyType`/`TriangleStrip` 参数） |
| 11 | cull / depth | `CULL_NONE` / 无深度 | 不设 cull / 无 depth 附件 | `cullMode NONE` / 无 depth 附件 | 管线创建参数 |
| 12 | `params1` 整数编码 | `(uint)x` 数值转换（禁 `asuint`） | `uint(p.params1.x)` | `uint(p.params1.x)` | `build.rs` 的 `FORBIDDEN` 列表 |
| 13 | 片元位置 | 不用 `SV_Position`，用插值 `local_px`/`world_px` | 同（`local_px`） | 同（`v_local_px`） | 三份 FS 的入参 |
| 14 | `aa_padding` 外扩 | `params0.w` | 同 | 同 | `contract.md` §5 + 编码器 |
| 15 | 视口 | 整面 | 整面 | 整面（**正**高度 viewport，不做负高度翻转） | §6.6 的 `cmd_set_viewport` |

**MSL 的两个额外陷阱（P12 只此一处）**：

1. `[[stage_in]]` 的结构体字段顺序必须与 MSL 顶点输出一致（`position` 必须在第一项）；
   `geometry.metal` 的三个 VS 共用 `GeomVsOut` ⇒ 改字段顺序必须三处同改。
2. `texture2d<float>` 采样 `R8Unorm` 图集时 `.r` 是 0..1 覆盖率；**不得**声明成 `texture2d<float, access::sample>` 以外的形态（v1 全是 sample）。

### 10.2 每后端实现期 checklist（14 条）

| # | 检查 | 不满足的后果 |
|---|---|---|
| 1 | `render/{macos,linux}/compositor.rs` 的文件头写明 §4.3 的 `FramePlan` 语义差异 | 后来者按 `copy_plan()` 做局部 copy ⇒ 花屏（Metal/Vulkan 内容无保证） |
| 2 | 每个 `unsafe` 块有 `// SAFETY:` 注释；`ObjC` 指针/生命期写清 | clippy `undocumented_unsafe_blocks`（若开）与审查阻塞 |
| 3 | persist / swapchain 格式**同源**（mac 都是 `BGRA8Unorm_sRGB`；linux 同取 surface 的 `*_SRGB`） | sRGB 编码两次或 copy 非法 |
| 4 | `swapchain` 与 `persist` 的尺寸都用**物理像素**；`Scale` 只在逻辑↔物理换算处出现 | 模糊/错位（I11） |
| 5 | `view.isFlipped == true`（macOS）且 `NSEvent.locationInWindow` → `convertPoint` 后直接用 | Y 翻转（最容易漏的一条） |
| 6 | Vulkan 的 barrier 表（§6.6）逐行落地；**没有**「顺手少一个 barrier」 | validation error / 花屏 |
| 7 | `scissor` 矩形先与 surface 求交、丢弃空矩形（复用 `plan()` 的夹紧） | Vulkan validation error（`scissor` 越界）/ Metal clamp 但画错 |
| 8 | 图集纹理的 `generation` 与 `AtlasAllocator` 对齐；重建时旧纹理走 `retire` | 悬垂句柄 / UAF |
| 9 | 实例/裁剪缓冲**只写不重建容量**（扩容才重建）；resize 不碰 | 违反 §2.9 的核心性能前提 |
| 10 | `readback` 的 BGRA→RGBA 只做一次；行 pitch 按平台各自规则（mac `w*4`，linux tightly packed） | 快照通道错色 |
| 11 | 三平台 `Section::{Upload,Gpu,Present}` 同名同序 | `FrameRecord` 断言跨平台不可比 |
| 12 | `Renderer::submit` 的 `Err` 不吞、不重试（`lost` 粘性） | device lost 后继续提交 |
| 13 | 平台回调（`GuiView` / wayland handlers）**不碰 `App`**，只入队 | 借用冲突 / 重入 UB |
| 14 | `present` 之后的 `fresh` 清除只在**成功**后 | 首帧之后一直全量（性能）或半帧（花屏） |

### 10.3 私有性与模块根（机械检查）

```powershell
# 1) render.rs / platform.rs / render/{macos,linux}.rs / platform/{macos,linux}.rs 只允许 mod / pub(crate) mod / pub(crate) use
Select-String -Path crates/kytrix_gui/src/render.rs,crates/kytrix_gui/src/platform.rs `
  crates/kytrix_gui/src/render/macos.rs,crates/kytrix_gui/src/render/linux.rs `
  crates/kytrix_gui/src/platform/macos.rs,crates/kytrix_gui/src/platform/linux.rs `
  -Pattern '^(?!\s*//)(?!.*\b(mod|use)\b).+'          # 期望无输出

# 2) 没有 #[allow] / #[expect]
Select-String -Path crates/kytrix_gui/src -Recurse -Pattern '#\[(allow|expect)'   # 期望无输出

# 3) cfg 分派：三后端各一份，别留下 #[cfg(windows)] 的共享层
Select-String -Path crates/kytrix_gui/src/render.rs -Pattern 'cfg\('
Select-String -Path crates/kytrix_gui/src/window.rs -Pattern 'cfg\(windows\)' -Context 0,1
```

---

## 11. 测试与门禁

### 11.1 测试策略（**不写 mac/Linux 运行时测试**）

| 层 | P12 做什么 | 不做什么 |
|---|---|---|
| 单元测试（默认 `cargo test -p kytrix_gui`） | **零新增**；既有 589 项逐项不变（P11 基线） | 不写任何需要 GPU / 窗口 / Wayland 的用例 |
| `--features test-support` | 621 项 + 集成 6 项不变；`TestApp::snapshot` 在 mac/Linux 仍 `SnapshotError::NoRenderer`（S-124） | 不接 `Renderer::readback` 到快照（V-82） |
| 编译门禁 | §11.2 的两 target `cargo check`（唯一静态验收） | 不跑 `cargo test --target aarch64-apple-darwin`（无运行环境，且会链接失败） |
| 代码形态 | §10.3 的三条 grep + §10.2 的 14 条 checklist | 不靠「看起来对」 |

> **`readback.rs` 的编译覆盖**：`#[cfg(any(test, feature = "test-support"))]` ⇒ 在默认 check 里**不编译**。
> 为让它进静态门禁，实现期额外跑一次
> `cargo check -p kytrix_gui --target {aarch64-apple-darwin, x86_64-unknown-linux-gnu} --features test-support`
> （不链接、不运行），列入 §11.2。这是 mac/Linux 读回路径唯一可得的验证。

### 11.2 门禁命令（Windows 宿主；逐条给定 exit code 期望）

```powershell
# A) 默认测试不变（Windows 宿主，唯一可运行平台）
cargo test -p kytrix_gui                                  # 589 passed / 1 ignored，数量必须不变
cargo test -p kytrix_gui --features test-support           # 621 + 集成 6
cargo test --release -p kytrix_gui --features test-support
cargo test -p kytrix_gui_demo                              # 1

# B) lint / 格式 / 文档 / release（Windows 宿主）
cargo +nightly clippy -p kytrix_gui --all-targets -- -D warnings
cargo +nightly clippy -p kytrix_gui --all-targets --all-features -- -D warnings
cargo +nightly fmt --all -- --check
cargo doc -p kytrix_gui --no-deps
cargo build -p kytrix_gui --release

# C) 交叉 target 编译门禁（P12 的核心交付）
$env:KYTRIX_GUI_SKIP_SHADERS = "1"
cargo check -p kytrix_gui --target aarch64-apple-darwin                     # exit 0
$env:PKG_CONFIG = "$env:TEMP\p12-probe\pkg-config.exe"                      # stub（§2.2）
$env:PKG_CONFIG_ALLOW_CROSS = "1"
cargo check -p kytrix_gui --target x86_64-unknown-linux-gnu                  # exit 0
cargo check -p kytrix_gui --features test-support --target aarch64-apple-darwin        # exit 0
cargo check -p kytrix_gui --features test-support --target x86_64-unknown-linux-gnu    # exit 0
Remove-Item Env:KYTRIX_GUI_SKIP_SHADERS,Env:PKG_CONFIG,Env:PKG_CONFIG_ALLOW_CROSS

# D) 依赖纪律
cargo tree -p kytrix_gui --target x86_64-unknown-linux-gnu | Select-String 'x11|xcb|winit|wgpu|naga'      # 期望无输出
cargo tree -p kytrix_gui --target x86_64-unknown-linux-gnu | Select-String 'wayland-sys'               # 期望**有**输出（F-135）
git diff --stat -- Cargo.lock                                # 期望 +2 行（dispatch2 依赖边、dispatch2 的 libc）
git diff -- Cargo.toml crates/kytrix_gui/Cargo.toml          # 期望只有 dispatch2 / system feature 两处
```

**可接受的 exit code 偏差**（必须写进实现记录，不能静默）：

| 情况 | 期望 |
|---|---|
| Linux check 缺 stub pkg-config | exit 1 且**唯一**失败点是 `yeslogic-fontconfig-sys` 的 build script（第三方）；**不得**出现任何 `kytrix_gui` 自身错误 |
| 缺 `xcrun` / `glslangValidator` | `build.rs` 打印 skip warning + 写占位；`.metallib`/`.spv` 不进产物校验 |
| 非 Windows target 的 `dead_code` | **实测已归零**（§2.2 的 60 项基线由新后端消费掉）；P12 新增文件**不得**引入新的 `never used` |

### 11.3 实现后实测（本机 Windows / rustc 1.97.1，`KYTRIX_GUI_SKIP_SHADERS=1`）

| 门禁 | 结果 |
|---|---|
| `cargo check --target aarch64-apple-darwin` | exit 0，**0 warning** |
| `cargo check --target x86_64-unknown-linux-gnu`（stub pkg-config） | exit 0，**0 warning** |
| `cargo check --target aarch64-apple-darwin --features test-support` | exit 0，仅 1 项既有 warning（`render::frame::reserve` 无消费者） |
| `cargo check --target x86_64-unknown-linux-gnu --features test-support` | exit 0，同上 1 项 |
| `cargo test -p kytrix_gui` | 589 passed / 1 ignored（与 P11 基线一致） |
| `cargo test -p kytrix_gui --features test-support` | 621 passed / 1 ignored + 6 integration |
| `cargo +nightly clippy -p kytrix_gui -- -D warnings` | exit 0（工作区自定义 lint 全过） |
| `cargo +nightly fmt --all -- --check` | exit 0 |
| `cargo doc -p kytrix_gui --no-deps` | exit 0 |
| `cargo tree` | 无 `x11*` / `xcb*` / `winit`；`wayland-sys` 按 F-135 存在 |
| `git diff Cargo.lock` | +2 行（`dispatch2` 依赖边 + 其 `libc`），无版本漂移 |


---

## 12. 风险与交接

| # | 风险 | 影响 | 缓解 / 归属 |
|---|---|---|---|
| R-1 | **L5 实机矩阵完全缺失**（GNOME / KDE / Sway × 单/多 GPU × 多显示器 × 分数缩放 × IME） | 高：Wayland/AppKit 的行为差异无法静态发现 | V-82：P12 交付「可编译 + 结构正确」，出帧验证归 L5；本文 §7.8 / §8.8 的映射表就是 L5 的对照清单 |
| R-2 | **MSL / SPIR-V 未在本机编译**（无 `xcrun` / `glslangValidator`） | 中：shader 写错要到 CI 才发现 | `build.rs` 已有三平台编译 + magic 校验；CI 必须加 macOS runner（`xcrun metal`）与 Linux `glslang-tools`（V-83 的一部分） |
| R-3 | **objc2 双版本**（`0.5.3`（accesskit_macos）+ `0.6.5`（引擎）） | 中：`NSPoint`/`NSArray`/`NSObject` 跨版本不可直接互传 | 已按「裸指针 + `Retained::retain` + `cast`」隔离（§7.7）；`accesskit_macos` 升级到 0.6 线时删掉 `cast` 与 `retain` 包装（一次性重构，不留兼容层） |
| R-4 | **Metal argument buffer / 纹理槽上限**（未来 ≥3 页图集） | 低（v1 只用 2 张纹理） | §5.10 的分批策略；D8 保留 Tier2 检测；v1 不引 argument buffer（F-119） |
| R-5 | **分数缩放 + viewporter 的 compositor 实现差异**（GNOME 的 `preferred_scale` 与 buffer 尺寸约束） | 中：模糊或过扫描 | §8.2 的「物理尺寸 = ceil(logical × scale) + viewporter destination = logical」；V-83 要求三 compositor 实测 |
| R-6 | **`VK_KHR_incremental_present` 支持率**（presentation engine 可忽略） | 低：仅省电/省带宽的 hint | §0 #10：引擎不依赖；`present_dirty()` 只喂 hint |
| R-7 | **整面 copy 的带宽成本**（两个新后端每帧写满，D3D12 是局部） | 中：4K/高刷下带宽翻倍 | 这是「无局部 present」的必然代价（主设计 §2.5 已接受）；L4 基准量化（V-91）；优化方向只有「跳过整帧」（已有） |
| R-8 | **macOS 自绘标题栏后交通灯只剩关闭键** | 低：用户失去最小化/最大化按钮 | F-127：P13 的窗口控件必须补等价按钮；v1 保留 `Cmd+M` / 双击缩放的系统行为 |
| R-9 | **Wayland 剪贴板的异步语义**与同步 API 的错配 | 中：首次粘贴拿到 `Err(Clipboard)` | F-130 已在 `App::clipboard_text` 文档写明；`cached` 命中后正常 |
| R-10 | **IME 差异**（macOS 首响应者、Wayland `done` 双缓冲、serial） | 中：组合串闪烁 / 丢字 | §7.5 / §8.5 的状态机表；V-84 / V-88 列 L5 |
| R-11 | **`framebufferOnly = false` 的性能代价** | 低 | 若 L5 测得 `blit` 在 `framebufferOnly = true` 下可用，改回 `true`（一行） |
| R-12 | **`render/device.rs` 拆分的机械改动面**（6 个 Windows 单测搬移） | 低：纯搬移 | S-119：一次完成，不同时改语义；`cargo test` 数量不变是判据 |

**交接清单（给 P13 / L5 / P18）**：

1. P13：`MTLSharedEvent` 异步化（用 `msg_send!` 或升 objc2）；窗口控件（最小化/最大化按钮）；Linux 光标形状（`wl_cursor` 需要新依赖，需裁决）。
2. L5：V-82 的矩阵；§7.8 / §8.8 映射表逐条实测；R-5 / R-11 的量化。
3. P18：像素 golden 需要 mac/Linux 的 `TestApp::snapshot`（当前 `NoRenderer`）⇒ 先做 V-82 再谈 golden。

---

## 13. 需裁决项（5 条，含建议与代价）

| # | 议题 | 建议 | 备选与代价 |
|---|---|---|---|
| **D-1** | `render/device.rs` 的 `GpuCaps` 是 D3D12 专用结构，三平台语义不同 | **拆**：`GpuCaps`/`fl_name`/`sm_name` 搬进 `render/windows/device.rs`；`render/device.rs` 只留三个 `D8_REQUIRED_*` 常量；mac/Linux 各自定义同名同形的 `GpuCaps` | 保留共享 `GpuCaps` 并把 mac/Linux 字段全塞进去：一个 20+ 字段、每平台 2/3 恒零值的结构，`actual()` 无法稳定格式化（违反 AGENTS 1.2）。代价：搬 6 个既有单测（纯搬移） |
| **D-2** | Metal 加载 metallib 需要 `DispatchData` | **加 `dispatch2 = "0.3.1"` 为直接依赖**（workspace + kytrix_gui 各一行）：已在 `Cargo.lock`、已由 objc2-metal 的 default features 启用 ⇒ **零新 crate、零 lock 变化**，`newLibraryWithData` 直接可用 | ① 临时文件 + `newLibraryWithURL`（零依赖，但每次启动写 ~500 KB 到 `%TEMP%`，且要处理清理与错误）；② `newLibraryWithFile` 已 deprecated。推荐 ① 之外的**唯一**候选是加依赖 |
| **D-3** | Metal 帧同步：`waitUntilCompleted`（同步）还是 `MTLSharedEvent` | **v1 同步 `waitUntilCompleted`**（与 D3D12 的提交+等待同构；不需要协议上转） | `MTLSharedEvent`：`encodeWaitForEvent:` 需要 `&ProtocolObject<dyn MTLEvent>`，objc2 0.6.5 无上转 ⇒ 必须 `msg_send!`（一处 unsafe，可接受），但同步提交下**没有任何收益**。P13 异步化时再上 |
| **D-4** | Linux D8 是否把 `descriptorIndexing` 列为**硬基线**（v1 的 shader 只用固定 descriptor set，不用 runtime array） | **列为硬基线**（与主设计 §2.2 的 `required` 文案逐字一致；Vulkan 1.2 桌面驱动普遍支持，拒绝面极小） | 降为「记录项」：错误文案就要改写主设计 §2.2 的字符串，且未来图集分批启用 runtime array 时要重新收紧基线（两处真值）。代价：极少数 Vulkan 1.2 精简驱动会被拒 |
| **D-5** | Wayland 分数缩放：`wp_viewporter` + `set_destination` 还是只做整数 `ceil(scale)` | **viewporter + destination**（协议推荐做法；否则 GNOME 的 1.25/1.5 会模糊） | 只 ceil：少一个协议对象，但非整数缩放下被 compositor 二次缩放出模糊；且 `preferred_scale` 的语义就是让客户端按 1/120 步进渲染。代价：viewporter 是 stable 扩展，缺失时回退整数 `wl_output.scale`（§8.2 已写） |

---

## 14. 本阶段追加到 `design/DEVIATIONS.md` 的条目（§十七）

> 实现期若发现 §14 的某条不成立，**改这一节 + 实现**，不留「文档与代码两套真值」。

| 编号 | 内容 |
|---|---|
| **F-119** | **macOS 不做 argument buffer**：P07 已落地的 `contract.md` §7 绑定表是 `buffer(0/1/2)` + `texture(0/1)` + `sampler(0)` 直绑；改成 argument buffer 要改 14 个 shader 源文件 + `contract.md` + `build.rs` 锚点，且 D3D12/Vulkan 无对应物。`argumentBuffersSupport == Tier2` 仍作为 D8 硬基线（主设计 §2.2），未来 ≥3 页图集时按 `texture_index` 拆 draw（不是塞 argument buffer 数组） |
| **F-120** | **Metal v1 同步提交**：`MTLCommandBuffer::commit` + `waitUntilCompleted` 即 fence；**不用** `MTLSharedEvent`——`encodeWaitForEvent:value:` 需要 `&ProtocolObject<dyn MTLEvent>` 上转，而 `objc2 0.6.5` 的 `ProtocolObject` 只有 `from_ref/from_id/from_retained`（probe `E0308`）。P13 异步化时用 `msg_send!` 或升级 objc2 |
| **F-121** | **`FramePlan` 的 `copy_plan()` / `present_dirty()` 是 D3D12 专用**：Metal 的 drawable 与 Vulkan 的 swapchain image **present 后内容未定义** ⇒ 两个新后端每帧整面 blit / `cmd_copy_image`；`plan()`（scissor）与 `is_full()`（clear）仍是共用语义。`render/{macos,linux}/compositor.rs` 文件头必须写明 |
| **F-122** | Metal 加载 metallib 用 `MTLDevice::newLibraryWithData:error:`，需要 objc2-metal 的 **`dispatch2` feature**（其 default features 已启用）与 **`dispatch2` 直接依赖**（`dispatch2 0.3.1` 已在 `Cargo.lock`，由 objc2 生态引入 ⇒ **零 lock 变化**）。`newLibraryWithFile` 已 deprecated；`newLibraryWithSource` 无法处理 `#include "common.metal"` |
| **F-123** | Vulkan v1 **不使用** `runtimeDescriptorArray`：GLSL 的 `instances[]` 是 std430 结构体数组（`binding 0` 的普通 storage buffer），不是 descriptor array。`descriptorIndexing` 仍按 D-4 作为硬基线；`runtime_descriptor_array` 只记录，不支持时降级 `false` 不失败 |
| **F-124** | `accesskit_unix 0.24.1` 与 P11 §12 的措辞不同：①`request_initial_tree() → None` **合法**（`context.rs:91-109` 进 `Pending`，下一次 `update_if_active` 必须是全树）；②`update_if_active` **无返回值**（无需 `raise()`）；③`DeactivationHandler` 由**库自己**在 deactivate / `RemoveAdapter` 时调用，引擎只实现空 handler。**不需要**新增 `full_update` 辅助（`A11yBridge` 的第一次 `take` 必为全树） |
| **F-125** | `RUST_FONTCONFIG_DLOPEN=1` **不能**作为 Linux 交叉 check 的绕过：`yeslogic-fontconfig-sys` 在 dlopen 形态下不在根导出 `fontique 0.12` 需要的 29 个 `Fc*` 符号（`E0432`）。可行解法只有 stub `pkg-config`（§2.2）或真 sysroot |
| **F-126** | Vulkan 用**核心 `VkRenderPass` + `VkFramebuffer`**（persist 一个附件）而非 dynamic rendering：`VK_KHR_dynamic_rendering` 在 1.2 是扩展，不是基线。swapchain image 不建 framebuffer（只作 `cmd_copy_image` 目标） |
| **F-127** | macOS 自绘标题栏：`styleMask = Titled|Closable|Miniaturizable|Resizable|FullSizeContentView`，`titlebarAppearsTransparent` + `titleVisibility = Hidden`；交通灯**只保留 Close**（重排到 x=12），Miniaturize/Zoom 隐藏 ⇒ P13 必须补引擎绘制的最小化/最大化控件，否则用户失去入口 |
| **F-128** | `zwp_text_input_v3` 的 `preedit_string` / `commit_string` / `delete_surrounding_text` 必须**缓存到 `done` 再原子生效**（可能同批到达）；`commit` 后要吞掉同帧紧随的字符键（GNOME 的部分版本会重复发） |
| **F-129** | `wl_data_device.set_selection` **必须**带最近的输入事件 serial（`wl_pointer::Enter` / `wl_keyboard::Enter` 的 serial）；无 serial 时 compositor 静默忽略 |
| **F-130** | Linux 剪贴板是**异步拉模型**：`App::clipboard_text()` 只能返回事件循环里缓存的最近一次选区内容，从未收到时返回 `Err(Clipboard)`；与 Win32/macOS 的同步语义不同，必须在公开 API 文档写明 |
| **F-131** | Metal 的 `Section::Gpu` 用 `MTLCommandBuffer::GPUStartTime/GPUEndTime`（command buffer 级），**不用** `MTLCounterSampleBuffer`（`counterSets` 可能为空、需拆 pass、CI 无法验证） |
| **F-132** | P01 的 objc2 feature 列表是**文档性冗余**：`objc2-app-kit` / `objc2-metal` 的 `default` 已包含 `NSButton` / `NSCursor` / `dispatch2` 等全部类与协议（probe 用 `cargo tree -e features` 验证）⇒ P12 **不需要**新增任何 feature |
| **S-119** | `render/device.rs` 拆分：`GpuCaps` / `fl_name` / `sm_name` / `FEATURE_LEVEL_12_0` / `SHADER_MODEL_6_2` 与 5 个单测搬入 `render/windows/device.rs`；共享文件只留 `D8_REQUIRED_{WINDOWS,MACOS,LINUX}` |
| **S-120** | `platform/event.rs`（`PlatformEvent` / `translate` / `key_from_vk`）**保持 `#[cfg(windows)]` 不重构**：mac/Linux 的平台层直接把原生事件翻成公共 `Event` 后调 `App::dispatch`（与 Win32 的 `to_app_event` 同层职责） |
| **S-121** | `ReadbackImage` 三个后端各一份（`render/{windows,macos,linux}/readback.rs`），**不**提到共享层：它只在 `test`/`test-support` 构建存在，且每个后端都要和自己的 `Renderer::readback` 同形 |
| **S-122** | `Application::run(self, app: App)` 三平台同签名：macOS **不子类化** `NSApplication`（帧泵挂 `CFRunLoopSource`）；Linux 用 `calloop` + `WaylandSource`。两者都不引 libdispatch / 第二套事件循环 |
| **S-123** | P08 的 S-78（共享层只在 Windows 编译）**收口**：`render.rs` / `platform.rs` / `window.rs` / `app.rs` 里为「只有一个后端」而加的 `#[cfg(windows)]` 全部删除；`app.rs` 的 `Application` / clipbaord / `attach_a11y` / `upload_glyph_page` 一并解除 |
| **S-124** | mac/Linux 的 `TestApp::snapshot` 仍返回 `SnapshotError::NoRenderer`：`Readback` 与 `Renderer::readback` 照常按 `#[cfg(any(test, feature = "test-support"))]` 编译（§11.2-C 覆盖），但不接快照路径（V-82 之后由 P18 接） |
| **V-82** | **L5 实机矩阵**：GNOME 45+/KDE Plasma 6/Sway × 单/双 GPU（含 hybrid） × 单/多显示器 × 100%/125%/150%/175% × 中/日文 IME。P12 只保证「两 target 编译 + 结构正确」，出帧与像素归 L5 |
| **V-83** | MSL / SPIR-V **未在本机编译**：CI 必须加 macOS runner（`xcrun metal`）与 `glslang-tools`；`build.rs` 的 skip 分支只在缺工具链时用 |
| **V-84** | macOS dead key / IME 行为未验证（`NSEvent.characters` 是否已合成、`insertText:` 的 `replacementRange` 语义）；L5 用中日德文语料回放 |
| **V-85** | macOS 滚轮「行」换算（`scrollingDeltaY / 3.0`）未校准：L5 与 Win32 的 120 单位/行对齐 |
| **V-86** | 无 `zxdg_decoration_manager_v1` 时 compositor 可能自己加服务端装饰（与自绘标题栏重叠）；L5 在三大 compositor 上确认 |
| **V-87** | Linux 光标形状**未设置**（无 `wayland-cursor` 依赖）：`take_cursor_change()` 在 Linux 空转；是否需要新依赖由 P13 裁决 |
| **V-88** | 多 seat / 多键盘未支持（v1 只接第一个 `wl_seat`）；`zwp_text_input_v3` 的 seat 绑定同理 |
| **V-89** | xkb 修饰键位图的取法二选一（`serialize_mods` 位图 vs `xkb_state_mod_name_is_active`）未在本机验证；实现期以 probe 编译通过的写法为准，行为归 V-82 |
| **V-90** | Metal 的 Xcode GPU Frame Capture（`MTLCaptureManager`）与 Vulkan 的 `VK_EXT_debug_utils` 未接：v1 只靠 `EngineError` + 文件诊断 |
| **V-91** | 「每帧整面 copy/blit」的带宽成本未基准（4K/120Hz 下 persist→swapchain 约 1.99 GB/s）；L4 量化后再决定是否需要「多 swapchain image 各自缓存一份 persist」 |

---

## 附录 A · probe 骨架与复现

```powershell
# 目录：%TEMP%\kg-p12-macos-linux（独立 workspace；依赖逐字复制 P01 的声明）
$env:CARGO_TARGET_DIR = "$env:TEMP\kg-p12-macos-linux\target"
Set-Location "$env:TEMP\kg-p12-macos-linux"

cargo check --target aarch64-apple-darwin --offline            # exit 0
cargo check --target x86_64-unknown-linux-gnu --offline        # exit 0
```

| 文件 | 覆盖的设计面 |
|---|---|
| `src/macos.rs` | `MTLCreateSystemDefaultDevice` / `argumentBuffersSupport` / `CAMetalLayer` 参数 / `nextDrawable` / metallib 加载（`dispatch2`）/ 三份 PSO + vertex descriptor + blend / blit（buffer↔texture）/ `replaceRegion` / `GPUStartTime`·`GPUEndTime` / `defined_class!`（NSView 子类 + `NSTextInputClient` 9 方法 + `NSAccessibility` 4 方法）/ `NSWindow` + `standardWindowButton` / `CFRunLoopSource` / `NSPasteboard` / `accesskit_macos` 五个调用 |
| `src/linux.rs` | `Entry::load` / instance + `VK_KHR_wayland_surface` surface / `PhysicalDeviceVulkan12Features` / device + timeline / swapchain + formats + `PresentRegionsKHR` / descriptor set layout + push constants + 3 pipeline / atlas 上传 + barrier / persist + `cmd_copy_image` / `cmd_write_timestamp` / readback（`cmd_copy_image_to_buffer` + map）/ `registry_queue_init` + `globals.bind` + `xdg_*` + `wp_fractional_scale` + `zwp_text_input_v3` + `zxdg_decoration` + `wl_data_device` / `WaylandSource` + `calloop::ping` / `xkb::Keymap::new_from_fd` + `State` / `accesskit_unix::Adapter` |

**编译期的三个关键负例（指导实现，不要踩）**：

| 写法 | 结果 |
|---|---|
| `unsafe impl NSView for GuiView { … }` | `E0404: expected trait, found struct NSView` ⇒ 父类覆盖必须写在 `impl GuiView { #[unsafe(method(..))] }` |
| 返回对象的方法用 `#[unsafe(method(..))]` | `E0277: Retained<…>: Encode` ⇒ 必须 `#[unsafe(method_id(..))]` |
| `MTLCommandBuffer::encodeWaitForEvent_value(&shared_event, v)` | `E0308: expected &ProtocolObject<dyn MTLEvent>` ⇒ objc2 无协议上转（F-120） |

## 附录 B · API 事实速查（逐条 probe 编译验证）

| 面 | 事实 |
|---|---|
| `MTLDevice` | `MTLCreateSystemDefaultDevice() -> Option<Retained<ProtocolObject<dyn MTLDevice>>>`；`argumentBuffersSupport() -> MTLArgumentBuffersTier`（`Tier1` / `Tier2`）；`hasUnifiedMemory()`；`registryID()`；`maxBufferLength() -> NSUInteger` |
| `MTLCommandQueue` | `newCommandQueue()` **无参**（`commandQueue`）；没有 `setLabelsVisible`（那是 `MTLCommandBuffer`） |
| `MTLCommandBuffer` | `commandBuffer() -> Option<…>`；`renderCommandEncoderWithDescriptor(&pass)`；`blitCommandEncoder()`；`commit()`；`waitUntilCompleted()`；`status() -> MTLCommandBufferStatus`；`error() -> Option<Retained<NSError>>`；`GPUStartTime()/GPUEndTime() -> CFTimeInterval`；`presentDrawable(&dyn MTLDrawable)` |
| `MTLRenderCommandEncoder` | `endEncoding()` 来自 **`MTLCommandEncoder`**（必须 `use objc2_metal::MTLCommandEncoder;`）；`setScissorRect(MTLScissorRect)`；`setViewport(MTLViewport{originX,originY,width,height,znear,zfar})`（`f64`）；`drawPrimitives_vertexStart_vertexCount_instanceCount` 是 **`unsafe`** |
| blit | `copyFromBuffer_sourceOffset_sourceBytesPerRow_sourceBytesPerImage_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin`（注意是 `sourceSize`，不是 `destinationSize`）；`copyFromTexture_…_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage` |
| 纹理 | `MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped` 是 **`unsafe`**；`replaceRegion_mipmapLevel_withBytes_bytesPerRow(region, level, NonNull<*mut c_void>, row_bytes)`；`contents() -> NonNull<c_void>`（`MTLBuffer`） |
| 像素格式 | `MTLPixelFormat::BGRA8Unorm_sRGB`（`sRGB` 大写 R/G/B） |
| `CAMetalLayer` | `CAMetalLayer::new()`；`setDevice(Option<&ProtocolObject<dyn MTLDevice>>)`；`nextDrawable() -> Option<Retained<ProtocolObject<dyn CAMetalDrawable>>>`；`CAMetalDrawable::texture() -> Retained<…dyn MTLTexture>`（**非 Option**）；`drawable.present()`（`MTLDrawable` 继承方法）；无 `ProtocolObject<dyn CAMetalDrawable> → dyn MTLDrawable` 的 Rust 上转 |
| `objc2` 类定义 | `#[unsafe(super(NSView))] #[thread_kind = MainThreadOnly] #[name = "…"] #[ivars = …]`；覆盖写在 `impl T {}`；协议 `unsafe impl P for T {}`（**必需方法会编译期检查**） |
| CoreFoundation | `CFRunLoop::main() -> Option<CFRetained<CFRunLoop>>`；`.add_source(Option<&CFRunLoopSource>, Option<&CFRunLoopMode>)`；`.wake_up()`；`CFRunLoopSource::new(Option<&CFAllocator>, order, *mut CFRunLoopSourceContext) -> Option<…>`；`.signal()`；`CFRunLoopSourceContext { version, info, retain, release, copyDescription, equal, hash, schedule, cancel, perform }`（**无 `Default`**） |
| `accesskit_macos 0.27.2` | `unsafe fn new(view: *mut c_void, is_focused: bool, action_handler: impl 'static + ActionHandler)`（主线程，内部 `MainThreadMarker::new().unwrap()`）；`update_if_active(f) -> Option<QueuedEvents>`；`update_view_focus_state(bool) -> Option<QueuedEvents>`；`hit_test(accesskit_macos::NSPoint, &mut H) -> *mut NSObject`；`view_children(&mut H) -> *mut NSArray<NSObject>`；`focus(&mut H) -> *mut NSObject` |
| `accesskit_unix 0.24.1` | `Adapter::new(activation: impl ActivationHandler + Send, action: impl ActionHandler + Send, deactivation: impl DeactivationHandler + Send)`；`update_if_active(f) -> ()`；`update_window_focus_state(bool) -> ()`；`set_root_window_bounds(Rect, Rect)`；`Drop` 发 `RemoveAdapter` |
| ash 0.38 | `Entry::load()`；`ApplicationInfo::application_name(&CStr)`；`khr::wayland_surface::Instance::{new, create_wayland_surface}`；`khr::surface::Instance::{get_physical_device_surface_support, _formats, _capabilities}`；`khr::swapchain::Device::{new, create_swapchain, get_swapchain_images, acquire_next_image, queue_present}`；`PhysicalDeviceVulkan12Features::{timeline_semaphore, descriptor_indexing, runtime_descriptor_array}`；`PresentRegionsKHR` 的 `swapchain_count` 是**字段**（`pRegions` 由 `.regions(&[..])` 设）；`vk::Viewport` 的宽高是 `f32`；`cmd_write_timestamp(stage, pool, index)` 用 1.0 的 `PipelineStageFlags` |
| wayland-client 0.31 | 构造请求的参数顺序是**子对象在前、`&QueueHandle` 在后**：`wm_base.get_xdg_surface(&surface, &qh, ())`、`manager.get_fractional_scale(&surface, &qh, ())`、`manager.get_text_input(&seat, &qh, ())`、`manager.get_data_device(&seat, &qh, ())`、`wm_base.get_xdg_surface(..)`；无参构造是 `(&qh, ())`（`compositor.create_surface(&qh, ())`） |
| wayland-protocols 0.32.13 | `wp::fractional_scale::v1` 需 **`staging`**；`wp::text_input::zv3` 与 `xdg::decoration::zv1` 需 **`unstable`**；`xdg::shell` 为 stable（P01 已开这三个 feature） |
| xkbcommon 0.9 | `unsafe fn Keymap::new_from_fd(&Context, fd: OwnedFd, size: usize, KeymapFormat, KeymapCompileFlags) -> io::Result<Option<Keymap>>`；`State::new(&Keymap)`；`State::update_mask(depressed, latched, locked, depressed_layout, latched_layout, locked_layout)`；`key_get_utf8(&self, Keycode) -> String`；`key_get_one_sym(&self, Keycode) -> Keysym`；没有 `ModifiersState` 类型（用裸 `u32`） |
| calloop 0.14 | `EventLoop::try_new()` 的 `map_err` 收 **`calloop::Error`**；`insert_source(..)` 的错误是 `InsertError`（`{error:?}` 打印）；`calloop::ping::make_ping()`；`WaylandSource::new(conn, queue).insert(handle)` |

## 附录 C · 与上游文档的交叉引用

| 本文 | 上游 |
|---|---|
| §0 #4 / §4.3 / §5.9 | 主设计 §2.5（无局部 present）、§2.7（资源生命周期）、P08 §6.3（persist 为权威） |
| §0 #8 / §6.3 / §6.6 | 主设计 §2.6、P08 §5.3（格式/视图分离）与 F-80（sRGB 只编码一次） |
| §5.10 | 主设计 §2.5（argument buffer 上限）、§2.8（图集映射表） |
| §7.7 / §8.7 | 主设计 §7.6（D22 直连）、P11 §12（P12 交接）与 F-111 / F-124 |
| §7.1 / §8.1 | 主设计 §8.1（引擎自有事件循环）、§8.2（窗口/DPI）、P09 §5（泵与唤醒纪律） |
| §10.1 | P07 `contract.md`（唯一权威）、§2.11（三套 shader 漂移防线） |
| §11.2 | P09 §9.1（门禁命令）、DEVIATIONS V-04（Linux 交叉） |
| §13 D-1…D-5 | AGENTS 1.2 / 1.4、主设计 §2.2（D8 文案） |






