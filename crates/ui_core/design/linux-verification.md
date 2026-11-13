# Linux 平台验证记录（Ubuntu 26.04 / Vulkan lavapipe / Wayland）

> 目标：把 `ui_core` 在 Windows 上已完成的功能面逐项搬到 Linux 实机验证，发现并修复 BUG，
> 记录每个修复的决策依据。
>
> 本文是**执行记录**；与设计文档不同的实现事实按纪律登记到
> [`DEVIATIONS.md`](DEVIATIONS.md)（本轮新增 F-145 … F-150、S-135 … S-137、V-110 … V-113）。

---

## 0. 结论摘要

| 项 | 结论 |
|----|------|
| 验证前状态 | **Linux 后端在 HEAD 上从未编译通过**：`cargo check -p ui_core` 直接失败（B1），`build.rs` 的 glslang 调用必然 panic（B2），3 个 vertex stage 的 GLSL 必然编译失败（B3）。`DEVIATIONS.md` V-82 记的「mac/Linux 只做交叉 `cargo check`」与实际不符 |
| 修复后状态 | 编译、测试、clippy、fmt 全绿；真实窗口（Wayland + Vulkan/lavapipe）出帧、交互、IME、a11y、剪贴板、DPI、resize、关窗全部实测通过 |
| 发现 BUG | **11 项**（3 项致命阻塞、2 项运行期崩溃、3 项功能缺陷、3 项门禁/测试） |
| 修复方式 | 全部经用户逐项决策后实施（见 §3 的决策表） |
| 像素 golden | 新增 `tests/golden/lavapipe/` 13 份基线；与 `warp/` 的差异已量化（§5.3） |
| 平台门禁 | Windows 731 passed / 0 failed；Linux 675 passed / 0 failed（`--features "hud,test-support"`）；两平台 release 亦全绿 |

---

## 1. 验证环境

### 1.1 虚拟机

| 项 | 值 |
|----|-----|
| 发行版 | Ubuntu 26.04.1 LTS（`resolute`） |
| 内核 | `7.0.0-43-generic` x86_64 |
| CPU / 内存 / 磁盘 | 2 vCPU / 5.2 GiB / 98 GB（可用 81 GB） |
| GPU | VMware SVGA II Adapter（无原生 Vulkan 驱动） |
| Vulkan | `libvulkan1 1.4.341`，**lavapipe**（`lvp_icd.json`，Mesa 26.0.8 软件光栅） |
| 桌面 | GNOME Wayland（mutter，`wayland-0`），`Type=wayland`、`Active=yes` |
| Rust | stable 1.100.0 / nightly 1.102.0（`clippy` + `rustfmt`） |
| 构建依赖 | `pkg-config`、`libfontconfig1-dev`、`libxkbcommon-dev`、`libwayland-dev`、`glslang-tools 16.2.0`、`fonts-noto-cjk` |

### 1.2 测试用合成会话

mutter 不实现 `wlr-screencopy`，也没有输入注入接口。因此**嵌套一个 sway**（`WLR_BACKENDS=wayland`，
父合成器是 mutter）作为测试夹具：

- `grim` 截屏（wlr-screencopy）；
- `swaymsg seat - cursor set/press/release` 注入指针；
- `wtype` 注入键盘；
- `wayland-info` / `WAYLAND_DEBUG=1` 读协议流。

另外用 `WLR_BACKENDS=headless` + `WLR_LIBINPUT_NO_DEVICES=1` 的 sway 复现「seat 无任何能力」的场景（B5）。

### 1.3 源码部署

`git archive HEAD` → tar.gz（11.2 MB）→ 解包到 `~/kytrix`。不带 submodule（`ui_core` 不依赖
`llama.cpp`）。修复期间用增量 `sftp` 同步改动文件。

---

## 2. 发现的问题

### 2.1 致命阻塞（Linux 后端完全不可用）

#### B1 · `draw_ops` 返回类型写错，Linux 后端整体不编译

```
error[E0308]: mismatched types
   --> crates/ui_core/src/render/linux/compositor.rs:968:8
    |
968 |     Ok(())
    |     -- ^^ expected `u32`, found `()`
```

`fn draw_ops(...) -> Result<u32, EngineError>` 的收尾写成了 `Ok(())`。Windows / macOS 的同名函数都返回
`Ok(draw_calls)`。**影响**：`cargo check -p ui_core` 在 Linux 上直接失败，任何 Linux 工作都无法开始。

#### B2 · `glslangValidator` 的 `-I` 必须是紧贴形式

```
ui_core: 着色器编译失败（glslangValidator quad.vert.glsl）
  status: exit status: 1
  stdout:
-I<dir> include path must immediately follow option (no spaces)
```

`build.rs` 传的是 `.args([..., "-I"]).arg(shader_dir)`（两个参数）。glslang 要求 `-I<dir>` 紧贴。
**影响**：Linux 上 `build.rs` 必然 panic，即使源码本身正确也构建不出来。

复现（VM 实测）：

| 形式 | 结果 |
|------|------|
| `-I .`（分开） | `-I<dir> include path must immediately follow option (no spaces)`，exit 1 |
| `-I.`（紧贴） | 通过参数解析（随后才暴露 B3） |

#### B3 · `fwidth` 出现在 vertex 阶段，3 个 GLSL stage 编译失败

```
ERROR: ./common.glsl:79: 'fwidth' : no matching overloaded function found
ERROR: quad.vert.glsl:6: '' :  syntax error, unexpected LAYOUT, expecting RIGHT_PAREN or COMMA
```

`common.glsl` 被 7 个 stage 包含，其中 **3 个是 vertex**（`quad.vert` / `geometry.ui.vert` /
`geometry.textured.vert`）。GLSL 禁止在顶点阶段调用导数函数，glslang 对**整份编译单元**报错——
即使 `aa_coverage` 从未被顶点入口调用。最小复现：

```glsl
#version 450
layout(location = 0) in vec2 p;
float helper(float d) { return max(fwidth(d), 1e-5); }   // 从未调用
void main() { gl_Position = vec4(p, 0.0, 1.0); }
```
→ 同样报 `'fwidth' : no matching overloaded function found`。

**影响**：7 个 SPIR-V 里 3 个编不出来。HLSL（dxc）与 MSL（`xcrun metal`）在两个 stage 都允许导数，
所以这个问题**只在 Linux 暴露**，Windows 侧完全看不到。

### 2.2 运行期崩溃

#### B4 · `wl_data_device` 缺 `event_created_child`，在 mutter 上启动即 abort

```
thread 'main' panicked at wayland-client-0.31.15/src/event_queue.rs:129:9:
Missing event_created_child specialization for event opcode 0 of wl_data_device
...
thread caused non-unwinding panic. aborting.
```

`wl_data_device.data_offer` 会创建 `wl_data_offer` 子对象，wayland-client 要求在 `Dispatch` 实现里用
`event_created_child!` 给出它的 user data。mutter 在窗口创建时就发 `data_offer`（剪贴板 offer），
所以**在 GNOME 上 100% 崩溃**，且是 `abort`（不可 unwind）。headless sway 不发 offer，所以不会崩——
这也是静态检查与「只跑 sway」都发现不了它的原因。

#### B5 · 无条件 `get_keyboard` / `get_pointer`，seat 无能力时连接被服务端杀掉

```
wl_seat#5: error 0: wl_seat.get_keyboard called when no keyboard capability has existed
Error: Swapchain { detail: "get_physical_device_surface_formats: ERROR_SURFACE_LOST_KHR" }
```

`create_devices` 不看 `wl_seat.capabilities` 就直接 bind。headless sway（`WLR_LIBINPUT_NO_DEVICES=1`，
seat `capabilities: 0`）实测：连接被断开，随后 Vulkan 报 `ERROR_SURFACE_LOST_KHR`——错误信息指向
swapchain，真正的根因在几十行之外的平台层。

### 2.3 功能缺陷

#### B6 · `Swapchain::resize` 先建后销毁，首次 resize 必报 `NATIVE_WINDOW_IN_USE_KHR`

```
Error: Swapchain { detail: "create_swapchain_khr: ERROR_NATIVE_WINDOW_IN_USE_KHR" }
```

Vulkan 规定一个 surface 同一时刻只能被一个 swapchain 持有。原实现先 `Swapchain::new` 再销毁旧的，
必然撞这条规则。**影响**：窗口第一次 resize 就失败退出。

#### B7 · 关掉最后一个窗口后事件循环不返回，进程 71% CPU 空转

`Application::run` 的契约是「跑到没有窗口」，Windows（`should_exit`）与 macOS（`windows.is_empty()`）
都实现了，Linux 的 `event_loop::run` 只在 `xdg_toplevel.close` 时 `return Ok(())`，应用自己调
`App::close_window`（demo 的 Esc）之后循环继续空转。

实测（修复前）：按 Esc 后 `swaymsg -t get_tree` 里窗口节点已消失（协议流里 `wp_presentation.destroy`
/ `wl_buffer.destroy` 都已发生），但进程仍存活、CPU 71%。

#### B8 · 剪贴板发布在 `serial == 0` 时静默丢弃文本

```rust
let Some(text) = outgoing().lock().ok().and_then(|mut slot| slot.take()) else { return; };
self.published = Some(text);
if serial != 0 {
    self.device.set_selection(Some(&self.source), serial);
}
```

先 `take()` 再判 `serial`。启动后第一个循环还没有任何输入事件，`serial` 恒为 0 ⇒ 文本被取走但永不发布。
实测（修复前）：`set_clipboard_text("kytrix-linux-clip-probe")` 之后外部 `wl-paste` 读到
`Nothing is copied`。

### 2.4 门禁与测试

#### B9 · `explicit_family_changes_the_shape` 写死 Windows 字体名

```
different families must produce different advances:
[("Consolas", 129.13602), ("Times New Roman", 129.13602), ("Segoe UI", 129.13602)]
```

Linux 上这三族都不存在，全部 fallback 到同一字体，advance 完全相同。测试测的是「宿主有没有装这三个
Windows 字体」，不是设计意图（`TextStyle.family` 必须前置进 family 列表）。

#### B10 · `cargo +nightly clippy --all-targets --all-features -- -D warnings` 5 个 error

| 位置 | lint |
|------|------|
| `platform/linux/event_loop.rs:178` | `inline module found`（`mod calloop_wayland { ... }`） |
| `platform/linux/surface.rs:578` | `collapsible_if` |
| `render/linux/device.rs:193` | `unnecessary_to_string` |
| `render/linux/pipeline.rs:384` | `chunks_exact_to_as_chunks` |
| `render/linux/readback.rs:132` | `chunks_exact_to_as_chunks` |

#### B11 · `cargo +nightly fmt -- --check` 失败（4 处）

`render/windows/compositor.rs` 的 `.map_err(...)` 缩进不一致。**与 Linux 无关**，Windows 检出同样红——
说明这条门禁在 HEAD 上就没跑过。

---

## 3. 修复与决策记录

每一项修复方案都由用户逐项拍板，本节记录**选项与最终选择**。

| # | 决策点 | 选项 | 用户选择 | 实施 |
|---|--------|------|----------|------|
| B1 | 返回类型 | 唯一解 | 按唯一解修 | `Ok(())` → `Ok(draw_calls)` |
| B2 | glslang 调用 | 唯一解 | 按唯一解修 | `.arg(format!("-I{}", shader_dir.display()))` + 注释说明约束 |
| B3 | 求导函数归属 | A 拆 `common.frag.glsl` / B `#ifdef` / C 改 uniform | **A** | 新增 `common.frag.glsl`（`aa_coverage` + `clip_mask`），4 个 `.frag.glsl` 各 include 两份；`common.glsl` 只留顶点可见部分 |
| B4 | 剪贴板 | A 只补 `event_created_child` / B 整套重做 | **B**（并进一步限定范围，见下） | 整套重写 `platform/linux/clipboard.rs` |
| B5 | seat 能力 | A 按 capabilities 建、变化时重建 / B 缺能力即报错 | **A** | 记录 `seat_caps`，`create_devices` 幂等（补建缺失、释放多余），`wl_seat::Capabilities` 事件里重跑 |
| B6 | swapchain 重建 | A `oldSwapchain` / B 先销毁再建 | **A** | `SwapchainCreateInfoKHR::old_swapchain(old)`，创建成功后销毁旧的 |
| B7 | 循环退出 | A 循环顶部判窗口为空 / B 只处理 close 请求 | **A** | 循环顶部 `app.windows().next().is_none() → return Ok(())` |
| B8 | serial 丢弃 | A 先判 serial 再 take / B 加 pending 槽 | **A** | `if serial == 0 { return; }` 提前返回，文本留在 mailbox |
| B9 | 字体测试 | A 平台无关断言 / B 非 Windows skip / C 内嵌字体 | **A** | 改成断言 family 列表内容；另加一条「宿主真有两族时 advance 必须不同」的软跳过用例 |
| B10 | clippy | A 全部按建议改 / B 只修 Linux 侧 | **A** | 内联模块拆成 `event_loop/calloop_wayland.rs`；其余按建议改 |
| B11 | fmt | A 全部 fmt / B 只修 Linux 侧 | **A** | `cargo +nightly fmt -p ui_core`（含 Windows 那 4 处） |
| golden | Linux 像素基线 | A 实现离屏渲染器 / B 借真实窗口 / C 只记录 | **A** | 新增无 surface 的 `VulkanInstance::new_headless()` + `Renderer::with_headless()` |

### 3.1 B4 的进一步范围确认

用户在「整套重做」之下又逐条勾选了范围：

| 子项 | 是否包含 | 实施 |
|------|----------|------|
| ① 修崩溃 + 协议合规 | ✅ | `event_created_child!`；`wl_data_offer.accept(mime, ...)` 后才 `receive`（原先漏 `accept`，属协议违规）；处理 `wl_data_source.cancelled` 清空已发布文本 |
| ② mime 协商 | ✅ | 记录 offer 声明的 mime，按 `text/plain;charset=utf-8` > `text/plain` > `UTF8_STRING` 挑第一个双方都支持的；不再写死 `MIME_TEXT[0]` |
| ③ serial 生命周期 | ✅ | 即 B8 的修法；`serial == 0` 保留 mailbox |
| ④ 读回改成异步请求 API | ❌ 未选 | `App::clipboard_text()` 语义不变 |
| ⑤ primary selection | ❌ 未选 | 不接 `zwp_primary_selection_device_v1` |
| ⑥ 读回大小上限 | ✅ | `reader.take(1 MiB)` |
| ⑦ 只修最小面 | ❌ 未选 | — |

验收方式选 **A（双向实测）**：引擎 → 外部 `wl-paste`，外部 `wl-copy` → 引擎 `clipboard_text`。

---

## 4. 修复后的验证矩阵

命令在 VM 的 `~/kytrix` 下执行；`--features "hud,test-support"` 覆盖 demo 场景与 HUD。

### 4.1 构建与门禁

| 命令 | 结果 |
|------|------|
| `cargo check -p ui_core --all-targets --features "hud,test-support"` | 通过 |
| `cargo test -p ui_core --features "hud,test-support"` | **675 passed / 0 failed**（+ 集成 `frame_record` 6 / `hud` 7 / `layout` 4 / `snapshot` 8 / demo 36） |
| `cargo test -p ui_core --release --features "hud,test-support"` | 666 + 61 passed / 0 failed |
| `cargo +nightly clippy -p ui_core --all-targets --all-features -- -D warnings` | **通过**（0 error / 0 warning，除 workspace 级的 unused-dependency 提示） |
| `cargo +nightly fmt -p ui_core -- --check` | **通过** |
| `cargo doc -p ui_core --no-deps` | 通过 |
| `cargo bench -p ui_core --bench perf --no-run` | 通过 |
| shader 编译 | 7 个 SPIR-V 全部真实产出（magic `0x07230203`），非占位符 |

Windows 侧（同一份改动）：

| 命令 | 结果 |
|------|------|
| `cargo test -p ui_core --features "hud,test-support"` | **731 passed / 0 failed / 1 ignored** |
| `cargo test -p ui_core --release --features "hud,test-support"` | 722 + 61 passed / 0 failed |
| `cargo +nightly clippy ... -- -D warnings` | 通过（含此前红的 `render/windows/compositor.rs`） |
| `cargo +nightly fmt -- --check` | 通过 |

### 4.2 真实窗口与渲染

| 能力 | 方法 | 结果 |
|------|------|------|
| 真实窗口出帧 | 嵌套 sway 跑 `demo`，`grim` 截屏 | ✅ 1280×720，Vulkan/lavapipe，HUD 显示 `1.00x` |
| 整帧内容 | 截图逐块核对 | ✅ 六张卡片、CJK、色块、图片图集、滚动列表全部正确 |
| resize | `swaymsg resize set 1100 720` / `500 400` | ✅ 重排正确（修复前必失败，B6） |
| 全屏 | `swaymsg fullscreen enable/disable` | ✅ 1280×720 ↔ 恢复 |
| DPI | `swaymsg output WL-1 scale 2` / `1.5` / `1` | ✅ `preferred_scale` 120→240→180→120；HUD 显示 `2.00x` / `1.50x`，逻辑尺寸随之重排 |
| 空闲 0 帧 | 无 HUD，`WAYLAND_DEBUG` 统计 5 s 内 commit | ✅ **0 次 commit / 0% CPU**（设计目标达成） |
| 连续帧 | 带 HUD，同上 | 每秒约 180 commit（HUD 每帧刷新，符合 `needs_continuous_frames`） |
| 关窗退出 | Esc / `swaymsg kill` | ✅ 进程退出、窗口消失（修复前 71% CPU 空转，B7） |
| 离屏快照 | `snapshot_demo --snapshot` | ✅ 960×740 PNG，含 IME 组合串 / CJK / 图片（修复前恒 `UnsupportedGpu`） |

### 4.3 文本与 IME

| 能力 | 结果 |
|------|------|
| CJK 混排（简/繁/日/韩 + 英文） | ✅ 无豆腐块，`544px` 内自动折行 |
| ASCII 输入 | ✅ `wtype "hello"` → 输入框逐字符上屏，光标跟随 |
| 中文输入 | ✅ `wtype "中文测试"` → 上屏，`最近 IME 事件：Text("试")` |
| Backspace / Enter | ✅ 删除、提交（`最近提交：hello wor`） |
| 按键不丢 | ✅ 三种 `wtype` 速度（默认 / `-d 20` / `-d 60`）各 3 轮：协议层 `wl_keyboard.key` 均为 **10/10**（5 字符 × 按下+释放），输入框内容逐轮一致 |
| IME caret 定位 | ✅ `zwp_text_input_v3.enable` + `set_cursor_rectangle(84,536,1,23)`，随插入点移动（`56→65→66→84`） |
| 组合串 | ✅ `snapshot_demo` 的 `preview` 状态里 preedit「shi li」与已提交文本共存 |

### 4.4 交互与无障碍

| 能力 | 方法 | 结果 |
|------|------|------|
| Tab 焦点环 | `wtype -k Tab` ×3 | ✅ 焦点面板 `Tab 序 1/7 → 4/7`，焦点环跟随 |
| Enter 激活 | `wtype -k Return` | ✅ 触发 Click，`最近 AT 动作：Click → 选项 A` |
| 指针点击 | `swaymsg seat - cursor set/press/release` | ✅ 色块选中、`选中：色块 2（绿）· #30A46C` |
| 方向键 | `wtype -k Right` ×2 | ✅ 滑块 60% → 70% |
| F12 HUD | `wtype -k F12` ×2 | ✅ 隐藏 / 显示 |
| 滚轮滚动 | 按钮 5 ×5 / 按钮 4 ×3 | ✅ 视口按 24 px 行高精确位移（像素列采样核对） |
| AT-SPI 树 | `pyatspi` 走 `org.a11y.Bus` | ✅ frame + label / radio button / slider / entry / image，名称与描述齐全（需 `gsettings set org.gnome.desktop.interface toolkit-accessibility true`） |
| a11y 动作回投 | AT-SPI `do_action` | ✅ `Click` 回到主线程并改状态 |

### 4.5 剪贴板（B4/B8 的验收面）

| 方向 | 命令 | 结果 |
|------|------|------|
| 引擎 → 外部 | 引擎 `set_clipboard_text("kytrix-linux-clip-probe")`，外部 `wl-paste` | ✅ 读到 `kytrix-linux-clip-probe`（修复前 `Nothing is copied`） |
| 外部 → 引擎 | 外部 `wl-copy "from-outside"`，引擎下一帧 `clipboard_text()` | ✅ 读到 `from-outside` |
| 协议合规 | `WAYLAND_DEBUG` 看 offer 流 | ✅ `accept` 先于 `receive`；`cancelled` 清空已发布文本 |

### 4.6 seat 能力（B5 的验收面）

| 场景 | 结果 |
|------|------|
| 嵌套 sway（键盘 + 指针，`capabilities: 3`） | ✅ 全部输入功能可用 |
| headless sway（`capabilities: 0`） | ✅ 进程存活、窗口正常渲染（修复前 `wl_seat error 0` 断开） |

---

## 5. 像素 golden

### 5.1 目录与设备分组（D14）

`tests/golden/<device>/`：

| 目录 | 设备 | 状态 |
|------|------|------|
| `warp/` | Windows WARP | 既有 13 份 |
| `lavapipe/` | Linux Mesa llvmpipe | **本轮新增 13 份** |
| `metal-real/` | macOS 真实 GPU | 2026-11-13 由 macOS 实机补齐（见 [`macos-verification.md`](macos-verification.md)） |

`SnapshotCase::golden_path()` 按 `device_dir()` 取目录；三个平台都各自比对（`skip_reason()` 已由 S-138 删除）。

### 5.2 生成方式

```bash
UPDATE_GOLDEN=1 cargo test -p ui_core --features test-support --test snapshot -- --nocapture
cargo test -p ui_core --features test-support --test snapshot
```

13 个 case 全部 `max=0 avg=0.000 over=0`（写完立刻读回自检）。

### 5.3 lavapipe vs WARP 的量化差异

同一 case 在两份 golden 上的逐通道差：

| case | max | mean | 超 1 的像素 |
|------|-----|------|-------------|
| `clip_1_light` | 135 | 0.0115 | 16 / 38400 |
| `colors_1_dark` | 67 | 0.0204 | 29 / 19200 |
| `colors_1_light` | 131 | 0.0632 | 23 / 19200 |
| `gradient_1_light` | 135 | 0.0389 | 26 / 38400 |
| `image_1_light` | 64 | 0.0075 | 3 / 19200 |
| `image_2_light` | 64 | 0.0039 | 3 / 76800 |
| `shadow_1_light` | 64 | 0.0175 | 104 / 38400 |
| `shapes_1.25_dark` | 99 | 0.0108 | 56 / 60000 |
| `shapes_1.25_light` | 156 | 0.0080 | 61 / 60000 |
| `shapes_1_dark` | 102 | 0.0150 | 62 / 38400 |
| `shapes_1_light` | 164 | 0.0188 | 65 / 38400 |
| `shapes_2_dark` | 102 | 0.0039 | 52 / 153600 |
| `shapes_2_light` | 131 | 0.0048 | 83 / 153600 |

**读法**：差异集中在 AA 边缘与阴影渐变（`max` 上百、`mean` 0.004–0.06），是软件光栅的舍入差异，
不是布局或绘制指令漂移。**结论**：跨设备共用一份 golden 会永久红，D14 的「按设备分组」是必要的；
容差数值本轮**不冻结**（沿用 `Tolerance::default()`，只对本设备比对）。

---

## 6. 未覆盖 / 遗留

| 项 | 说明 |
|----|------|
| macOS `metal-real/` golden | 已由 macOS 实机补齐（13 份，见 S-138） |
| 真实输入法（拼音/日文）组合 | VM 装了 `ibus-libpinyin` / `fcitx5`，但 GNOME 会话未启用输入法；本轮用 `wtype` 直出字符 + `zwp_text_input_v3` 协议流验证 caret 定位，**组合串的真实输入法行为未验证** |
| 多显示器 / 混合 DPI | VM 单输出 |
| KDE / Sway 作为产品目标合成器 | 只验了 mutter（GNOME）与 sway（测试夹具） |
| AT-SPI 真实读屏器联动 | 只验了树结构与 `do_action`，未接 Orca |
| a11y 子节点顺序 | 引擎按 demo 建树顺序输出（① ④ ⑤ ⑥ ③ ②），与 Windows 一致；属应用侧布局，非 Linux 缺陷。**观察项**，未改动 |
| `Wayland` 的 `wl_output` 整数 scale | 只走 `wp_fractional_scale_v1`；无该扩展的合成器回退路径未实机验证 |

### 6.1 测试夹具的坑（不是引擎缺陷）

记录两处会让结论失真的夹具问题，避免后续误判：

1. **`swaymsg seat - cursor press/release` 两次调用之间可能有 ~100 ms 间隔**。早期脚本没等够，
   截图落在按下之前，看起来像「点击无反应」。加长 settle（press/release 各 0.2–0.3 s、之后 0.8 s）后
   每次都能命中。
2. **嵌套 sway 里 `resize set` 对平铺窗口无效**，要先 `floating enable`。否则窗口尺寸不变，
   会被误判成「resize 不生效」。
3. **mutter 不实现 `wlr-screencopy`**，`grim` 直连 GNOME 会失败；`org.gnome.Shell.Screenshot` 对
   非 portal 调用方返回 `AccessDenied`，portal 的 `Screenshot` 又需要交互授权。所以截屏统一走嵌套 sway。

---

## 7. 复现命令速查

```bash
# 环境
sudo apt-get install -y --no-install-recommends pkg-config libfontconfig1-dev \
  libxkbcommon-dev libwayland-dev glslang-tools fonts-noto-cjk vulkan-tools

# 门禁
cargo test -p ui_core --features "hud,test-support"
cargo +nightly clippy -p ui_core --all-targets --all-features -- -D warnings
cargo +nightly fmt -p ui_core -- --check

# 真实窗口（需要 Wayland 会话）
XDG_RUNTIME_DIR=/run/user/$(id -u) WAYLAND_DISPLAY=wayland-0 \
  cargo run -p ui_core --example demo --features hud

# 离屏快照
cargo run -p ui_core --example snapshot_demo --features test-support -- --snapshot out/demo.png

# 像素 golden
UPDATE_GOLDEN=1 cargo test -p ui_core --features test-support --test snapshot -- --nocapture
cargo test -p ui_core --features test-support --test snapshot
```
