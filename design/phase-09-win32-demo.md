# Phase 09 · Windows 平台层 + 最小 demo — 详细设计（v1）

> 依据：`custom-gui-engine-design.md` §1.4 / §1.5 / §8.1 / §8.2 / §8.3 / §9.2 / 附录 A；
> `design/phase-08-d3d12.md`（§5.3 `for_hwnd` / `Present1` / V-55 / V-57）、`design/phase-06-paint-runtime.md` §9；
> 已落地 `app.rs` / `window.rs` / `render/**` / `test_support/**`；DEVIATIONS F-70…F-85 / S-73…S-78 / V-55…V-59。
>
> 本文 §N 指本文件。本轮**只做设计 + 本机 Win32 probe**：不写仓库代码、不改 `Cargo.toml` / `Cargo.lock`、不 commit。
> probe 全部产物在 `%TEMP%\kg-p09-win32`（仓库外），复现见附录 B。

---

## 0. 结论摘要（先给定死项）

| # | 定死项 | 一句话依据 |
|---|--------|-----------|
| 1 | **`platform/` 私有（I9），`Event` / `InputEvent` 公开在 `src/event.rs`**；`platform::event` 只做 `WM_* → PlatformEvent → Event::Input` 的纯函数翻译 | 公开白名单要能命名 `Event`/`InputEvent`；`HWND`/`MSG` 不得出平台层；翻译层零 Win32 类型 → 可单测 |
| 2 | **唤醒合并**：`Dispatcher.wake()` 先 `pending.store(true)`、再 `PostMessageW(WM_APP+1)`，只有 `compare_exchange(false→true)` 成功者才 post | probe §7：`PostMessageW` 队列上限 **10,000**（第 10001 条 `0x80070718` / `GetLastError=1816`） |
| 3 | **窗口：初始隐藏 → device/swapchain → `ShowWindow`**；标题栏 P09 **用系统标题栏**（`WS_OVERLAPPEDWINDOW`，`WM_NCCALCSIZE` 交 `DefWindowProcW`），自绘标题栏留 P12 | probe §2/§8：默认非客户区 480×320 → 客户 462×273；自绘（返回 0）客户区 = 窗口矩形 802×602 |
| 4 | **resize = `ResizeBuffers` + 重建 persist，不重建 VBO/实例缓冲**；调用前必须 `allocator.Reset` + `list.Reset`（丢弃命令列表记录的 back buffer 引用），且**修掉 P08 的两处 `ManuallyDrop` COM 引用泄漏** | probe §11：泄漏时 `ResizeBuffers` 恒 `0x887A0001`；双层释放后同尺寸/变尺寸**都 `S_OK`**（V-55 在本机闭环，见 F-86） |
| 5 | **`WM_PAINT` 一律 `BeginPaint`/`EndPaint` 验证**，不做任何绘制 | probe §4：不验证时 100 ms 内 **24,576** 次 `WM_PAINT`（风暴） |
| 6 | **输入走 `TranslateMessage` + `WM_CHAR`**（死键/组合由 OS 完成一次），引擎不自己跑 `ToUnicodeEx` | probe §9：合成 `WM_KEYDOWN` 经 `TranslateMessage` 产生 `WM_CHAR(0x61)`；§14 的死键语义只在「不用 `WM_CHAR`」时才需要 |
| 7 | **DPI：物理→逻辑只经 `Scale::to_logical`**（唯一反向换算点）；`WM_DPICHANGED` 的 `wParam` 低/高 16 位 = `dpi_x`/`dpi_y`；非 96 dpi 必须用 `AdjustWindowRectExForDpi` | probe §3：frame 在 96 dpi 是 16×39，在 120 dpi 是 18×47；§9：`wParam=0x00C000C0` → `(192,192)` |
| 8 | **最小尺寸**由 `WM_GETMINMAXINFO.ptMinTrackSize` 强制（`SetWindowPos` 会被夹紧），并在 `WM_SIZE` 再夹紧一次（防御外部 `SetWindowPos`） | probe §8：`SetWindowPos(120×90)` → 实际 `220×160` |
| 9 | **P09 不装 `SetTimer`**（无动画）；P13 的 animation tick 用 `SetTimer(1ms)`（实测 16.0 ms）而非 `SetTimer(16ms)`（实测 24.6 ms） | probe §5 的实测周期 |
| 10 | **消息泵必须先 drain 再阻塞**：`PeekMessageW` 会**取出并消费** `WM_QUIT`，泵必须显式识别后退出，否则 `GetMessageW` 永久阻塞 | probe §15 的首轮事故（见 F-90）；修正后 `GetMessageW` 返回 0 |
| 11 | **demo 是独立 crate `crates/kytrix_gui_demo`**（workspace member、**不进** `default-members`），只有 `main.rs` + `scene.rs`；`--snapshot <png>` 走 `TestApp::snapshot`（WARP，无窗口） | 默认成员仍是 `crates/kytrix`；CI 无桌面也能出图 |
| 12 | **线程模型**：主线程 = 引擎自有消息泵，**不是** tokio worker；跨线程只用 `AppHandle`（`Handle::spawn` + `Dispatcher`）；主线程内**禁止** `tokio::spawn` / `Handle::current()` | probe §6：主线程阻塞在 `GetMessageW`，tokio 任务 `Handle::spawn` 150 ms 后 `PostMessageW` 唤醒（实测阻塞 162.9 ms，循环只返回 1 次） |

### 0.1 五条裁决（其余条目都在这五条之下）

| # | 裁决 | 被否掉的替代 | 依据 |
|---|------|--------------|------|
| **A1** | 公开 `event.rs`（`Event`/`InputEvent`…）+ 私有 `platform/`，翻译层是无 Win32 类型的纯函数 | 把 `Event` 放 `platform`（公开类型不可命名）；翻译写在 `wndproc` 里（不可单测） | I9 + 白名单 §1.3 + probe 全部消息可用 `SendMessageW` 合成 |
| **A2** | resize = `ResizeBuffers` + 重建 persist，**并先修 F-86 的 `ManuallyDrop` 泄漏** | ①重建整条 swapchain（probe 变体 26：`E_ACCESSDENIED`）；②接受 `0x887A0001` 当作环境限制（F-86 已证伪） | probe §2.7：修复后同尺寸/变尺寸都 `S_OK` |
| **A3** | P09 **用系统标题栏**（`WM_NCCALCSIZE` 交 `DefWindowProcW`） | 直接上自绘标题栏（要同时做 `WM_NCHITTEST`/拖拽/双击最大化，属 P12） | probe §8：两种模式的客户区都已实测 |
| **A4** | 字符输入走 `TranslateMessage` + `WM_CHAR`，引擎不跑 `ToUnicodeEx` | 自己维护 dead-key pending 状态（多一套状态机 + 与 IME 竞争） | probe §9/§14 |
| **A5** | 唤醒合并（`AtomicBool` + 首次 post）+ 泵 `drain → 处理 → 阻塞`，`WM_QUIT` 显式识别 | 每次事件都 `PostMessageW`（10,000 上限）；`GetMessageW` 单条阻塞（会漏排队事件） | probe §2.2 |

---

## 1. 范围、文件与非目标

### 1.1 本阶段交付（P09 实现期落地的文件）

```text
crates/kytrix_gui/src/
├── event.rs                        # 公开：Event / InputEvent / PointerButton / Key / Modifiers / KeyState / ImeEvent
├── platform.rs                     # 私有模块根：只 mod 声明 + Windows 别名
├── platform/
│   ├── dispatcher.rs               # Dispatcher（唤醒合并 + 用户事件队列）/ FrameWake / UserEvent
│   ├── event.rs                    # PlatformEvent + `translate(msg, wparam, lparam, state)` 纯函数（无 Win32 类型）
│   └── windows.rs                  # Windows 后端模块根（只 mod 声明）
│       └── windows/
│           ├── event_loop.rs       # RegisterClassW / CreateWindowExW / WndProc / 消息泵 / WM_APP 唤醒 / 退出
│           ├── window.rs           # NativeWindow：创建/显示/最小尺寸/系统标题栏/关闭/destroy
│           ├── dpi.rs              # GetDpiForWindow / WM_DPICHANGED / AdjustWindowRectExForDpi / 外观
│           ├── ime.rs              # WM_IME_* 最小集 + ImmSetCompositionWindow（P12 扩 TSF）
│           └── clipboard.rs        # OpenClipboard / Get|SetClipboardData(CF_UNICODETEXT)
crates/kytrix_gui_demo/
├── Cargo.toml
└── src/
    ├── main.rs                     # 参数解析 + tokio runtime + App::new(handle) + Application::run
    └── scene.rs                    # 建树（彩色 quad + 文本）、命中后改色、Esc 退出
```

**修改**：`lib.rs`（`pub mod event;` + `mod platform;`）、`app.rs`（`Application` / `AppHandle` / `App::handle` /
`App::drain_events` / `App::dispatch` / 退出条件）、`window.rs`（`set_renderer` 调用点 + `attach_native`）、
`render/windows/swapchain.rs`（`for_hwnd` / `validate` 去掉 `#[cfg(test)]`）、
`render/windows/compositor.rs`（`Renderer::new_for_hwnd` 去 `#[cfg(test)]` + `resize` 前 reset 列表）、
`render/windows/queue.rs`（`wait_idle` 之后追加 `reset_all_lists`）、`error.rs`（新增 `WindowCreate`）、
`test_support/{input,app}.rs`（改用公开 `event::InputEvent`，删掉重复定义）、`Cargo.toml`（workspace member 加 demo）。

### 1.2 明确的非目标

| 不做 | 归属 |
|------|------|
| 自绘标题栏 / `WindowControlArea` / 拖拽最大化 | P12 |
| 多窗口抽象、系统托盘、窗口分组 | P12（v1 只保证单窗口，主设计 §8.4） |
| 完整 IME/TSF（组合窗定位之外的候选窗、surrounding text） | P12 |
| 剪贴板富文本 / 图片 / 拖放 | P12 |
| `accesskit_windows` 接线（`WM_GETOBJECT`） | P10 |
| macOS（`NSApplication`/`NSTextInputClient`）与 Wayland（`calloop`/`zwp_text_input_v3`） | P11 / P17 |
| 动画 tick / 滚动惯性 | P13 |
| `Action` 分发 / 快捷键 | P10（P09 只把 `Event` 交给应用） |

---

## 2. probe（本机，2026-11-12）

> 目录 `%TEMP%\kg-p09-win32`（仓库外、独立 `target`，与 P08 probe 共享增量产物）。
> 依赖 `windows 0.62.2`（与 workspace 同版本）+ `tokio 1.54.1`（只 probe 用，`Cargo.lock` 不参与）。
> 三个入口：`kg-p09-win32`（平台全量）、`kg-p09-resize`（20 个 resize 变体，每个独立进程）、
> `kg-p09-resize2`（最终修复验证，单进程）。

### 2.1 环境

| 项 | 值 |
|---|---|
| OS / 会话 | Windows，**非交互式 agent 会话**；DWM 存在且**本窗口未被 cloaked**（`DwmGetWindowAttribute(DWMWA_CLOAKED)=0`） |
| GPU | `NVIDIA GeForce RTX 5080`（vendor 0x10DE / device 0x2C02 / 15977 MiB），`adapter[0]` 非软件适配器 |
| 进程 DPI | `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)` = ok；`GetDpiForSystem()=120`（scale **1.25**）；`GetDpiForWindow()=120` |
| tokio | multi-thread（worker=2）；主线程**不在** runtime 上下文里 |
| 窗口 | `RegisterClassW` atom `0xC649`；`WS_OVERLAPPEDWINDOW` 480×320 → 客户区 **462×273**，非客户区 **18×47** |

### 2.2 消息泵 / 唤醒 / 定时器（probe §4–§7、§15）

| 实测 | 结果 |
|---|---|
| `GetMessageW` 阻塞 | 主线程阻塞 **162.9 ms** 后收到 tokio 任务 `PostMessageW` 的 `WM_APP+1`（`wParam=0x5EED` 完整）；`GetMessageW` 循环**只返回 1 次** ⇒ 真正阻塞、不是轮询（I3） |
| `PostMessageW` 队列上限 | 连续 post 成功 **10,000** 次后失败：`0x80070718`（`GetLastError=1816 ERROR_NOT_ENOUGH_QUOTA`）⇒ **唤醒必须合并** |
| `SetTimer(16 ms)` | 320 ms 内 13 次 ⇒ 实测周期 **24.6 ms** |
| `SetTimer(1 ms)` | 320 ms 内 20 次 ⇒ 实测周期 **16.0 ms**（user32 定时器下限 ≈ 15.6 ms） |
| `KillTimer` | 之后 60 ms 内 `WM_TIMER` = 0 |
| `WM_PAINT`（可见窗口） | `InvalidateRect+UpdateWindow` → 2 次 `WM_PAINT`/`EndPaint`；验证后 30 ms 新增 0 |
| **`WM_PAINT` 不验证** | 100 ms 内 **24,576** 次 ⇒ 无限风暴（引擎必须每次 `BeginPaint`/`EndPaint`） |
| 隐藏窗口 | `InvalidateRect` **不产生** `WM_PAINT`（未显示前不会有绘制请求） |
| `WM_QUIT` | 被 `PeekMessageW` 取出后 `DispatchMessageW` **静默丢弃**；不显式识别则后续 `GetMessageW` 永久阻塞（首轮 probe 就是这样挂住的） |
| 关闭/退出 | `WM_CLOSE` → `DestroyWindow` → `WM_DESTROY` → `PostQuitMessage`；之后 `GetMessageW` 返回 **0**；`IsWindow` = false；`UnregisterClassW` = ok |

### 2.3 窗口 / DPI / 非客户区（probe §2–§3、§8、§10）

| 实测 | 结果 |
|---|---|
| 隐藏创建 | `CreateWindowExW`（无 `WS_VISIBLE`）成功；`IsWindowVisible=false`；`GWL_STYLE=0x04CF0000`（`WS_CAPTION`/`WS_THICKFRAME` 在） |
| `ShowWindow(SW_SHOWNORMAL)` | 返回 **false**（此前隐藏）→ `IsWindowVisible=true`；产生 `WM_SHOWWINDOW` / `WM_SIZE` / `WM_SETFOCUS` 各 1 次；`GetActiveWindow()==hwnd` |
| 非客户区（默认） | 96 dpi 换算：client 480×320 → window 496×359（frame **16×39**）；120 dpi：→ 498×367（frame **18×47**）⇒ **非 96 dpi 下必须用 `AdjustWindowRectExForDpi`** |
| `WM_NCCALCSIZE` | 默认模式到达 1 次（`wParam` 两种取值都会来）；`wParam=1` 时 `lParam` 是 `NCCALCSIZE_PARAMS*` |
| 自绘模式（返回 0） | 客户区 = 窗口矩形（802×602）⇒ 边界/标题栏可以做到 0 非客户区 |
| `WM_GETMINMAXINFO` | 写 `ptMinTrackSize=(220,160)` 后 `SetWindowPos(120×90)` → 实际 **220×160**（`SetWindowPos` **会**被夹紧；仍需在 `WM_SIZE` 二次夹紧） |
| `WM_DPICHANGED`（合成） | `wParam=0x00C000C0` → 低/高 16 位解出 `(192,192)` → scale 2.0；`lParam` 是建议 `RECT*`（只能同步 `SendMessageW` 传） |

### 2.4 输入映射（probe §9）

| 消息 | 实测解码 |
|---|---|
| `WM_MOUSEMOVE` | `lParam` LOWORD/HIWORD 按**有符号** 16 位解出 `(123,45)` |
| `WM_LBUTTONDOWN` | `wParam=MK_LBUTTON(0x1)`，位置同上 |
| `WM_MOUSEWHEEL` | `wParam` 高 16 位是**有符号** delta：`+120` / `-120`（`WM_MOUSEHWHEEL` 同为 `-120`）⇒ 方向 = `delta > 0` 向上/向右 |
| `WM_KEYDOWN` → `WM_CHAR` | 投递 1 条 `WM_KEYDOWN`，`TranslateMessage` **额外产生 1 条 `WM_CHAR(0x61='a')`** ⇒ 字符输入走 OS 翻译 |
| 焦点 / IME | `WM_SETFOCUS` / `WM_KILLFOCUS` / `WM_IME_STARTCOMPOSITION` / `WM_IME_COMPOSITION` / `WM_IME_ENDCOMPOSITION` 各 1 次；`GCS_COMPSTR=0x8`、`GCS_RESULTSTR=0x800` |

### 2.5 剪贴板 / IME / dead key（probe §12–§14）

| 实测 | 结果 |
|---|---|
| 剪贴板往返 | `OpenClipboard`→`EmptyClipboard`→`GlobalAlloc(GMEM_MOVEABLE)`→`SetClipboardData(CF_UNICODETEXT)`→`CloseClipboard`：写 `"Kytrix P09 clipboard 你好 🌍"`（54 B UTF-16），读回**逐字节相等**；序列号 3517→3522；`IsClipboardFormatAvailable` = true |
| 空串 | 只放 1 个 `NUL` 的 `HGLOBAL` 写入成功 |
| IME | `ImmGetContext` 返回**有效** `HIMC`；`ImmSetCompositionWindow(CFS_POINT 12,34)`=true；`ImmAssociateContextEx(hwnd,NULL,0)`=true；`ImmReleaseContext`=true；`ImmGetDefaultIMEWnd` 可拿 |
| 当前键盘布局 | 无死键（所有 `VK_OEM_*` 均返回 1 个字符） |
| 德语布局（`LoadKeyboardLayoutA("00000407")`） | `VK_OEM_5`（´）= **-1（死键）**；随后 `ToUnicodeEx(VK_A)` → `â`（**一次调用即组合**）；`VK_OEM_6` 一次返回 **2** 个码元 `^´` ⇒ 缓冲区必须 ≥ 8 u16、必须按返回值切片 |

### 2.6 D3D12 HWND swapchain + `Present1`（probe §11）

| 实测 | 结果 |
|---|---|
| `CreateSwapChainForHwnd`（隐藏窗口） | ok；`GetDesc1`：`format=87 (B8G8R8A8_UNORM)` / `bufferCount=3` / `SwapEffect=3 (FLIP_SEQUENTIAL)` / `Scaling=0 (STRETCH)` / `AlphaMode=3 (IGNORE)` / `flags=0` |
| ① 隐藏窗口 + 首帧全量 `Present1` | **`S_OK`**（`0x00000000`），设备 `S_OK` |
| ② `ShowWindow` 之后全量 `Present1` | `S_OK` |
| ③ 局部 dirty `(8,8)-(24,24)` `Present1` | `S_OK` |
| ④ 越界 dirty rect | `0x887A0001`（复验 P08 F-72 ②）；失败后设备未丢 |
| ⑤ present 之后 `ResizeBuffers` | **`0x887A0001`**（P08 V-55 的同一现象） |

> **①与 P08 V-57 的对照**：P08 在测试进程里隐藏窗口 present 失败（设备移除）。
> 本 probe（独立进程）隐藏窗口首帧 present **成功**，且随后显示/局部/越界三种调用行为与 P08 §2.6 完全一致
> ⇒ **V-57 是进程内状态污染（V-59 同源），不是"隐藏窗口不能 present"**。

### 2.7 `ResizeBuffers` 根因（P08 V-55 的闭环）

`kg-p09-resize` 用 20 个变体（每变体独立进程）穷举，结论：

| 变体 | 帧内容 | `ResizeBuffers` |
|---|---|---|
| 0 | 从不 present | `S_OK` |
| 1/2/3/4/5/7/8/9/10/11/12/13/14/15/16 | **只 `ClearRenderTargetView`**（隐藏/可见、sync 0/1、立即/延迟 200 ms、`DwmFlush`、FLIP_DISCARD、dirty present、常驻 persist+RTV 堆） | 全部 `S_OK`（且可重复 resize） |
| 17/18/19/21/22/23/24/25 | 任何把 back buffer 写入的 **`CopyTextureRegion` / `CopyResource`**（含 `allocator.Reset`+`list.Reset`、`ManuallyDrop` 单层释放、FLIP_DISCARD） | 全部 `0x887A0001` |
| 6 | `FRAME_LATENCY_WAITABLE_OBJECT` 创建 swapchain | 创建即 `0x887A0001`（本机该 flag 不可用） |
| 26 | 拷贝帧 → drop swapchain → 同 HWND 新建 swapchain | 新建 `0x80070005 E_ACCESSDENIED`（旧 chain 仍被泄漏的 back buffer 引用钉住） |
| **`kg-p09-resize2`（修复后）** | 拷贝帧 + **双层 `ManuallyDrop` 释放** + `allocator.Reset`+`list.Reset` | **`S_OK`**；随后 `Present1` = `S_OK`；**变尺寸** `ResizeBuffers(0, 624, 385, UNKNOWN, 0)` = **`S_OK`** |

**根因（定死）**：`windows 0.62.2` 把 `D3D12_RESOURCE_BARRIER` 的 union 载荷
（`ManuallyDrop<D3D12_RESOURCE_TRANSITION_BARRIER>`）以及其内层 `pResource`
（`ManuallyDrop<Option<ID3D12Resource>>`）都做成 `ManuallyDrop`；**结构体 drop 不会释放 COM 引用**。
P08 的 `transition()` 每帧对 back buffer 做 2 次 barrier、`copy_rect()` 每帧对 back buffer 做 1 次
copy-location，于是每帧泄漏 3 个 back buffer 引用，64 B/帧以上的引用计数永远不归零 ⇒
DXGI 判定"back buffer 仍被引用" ⇒ `ResizeBuffers` 一律 `0x887A0001`。

**修复（P09 必须做，两处，逐字）**：

```rust
// ① 每次 ResourceBarrier 之后（transition 助手内）：
let transition: &mut D3D12_RESOURCE_TRANSITION_BARRIER = &mut barrier.Anonymous.Transition;
std::mem::ManuallyDrop::drop(&mut transition.pResource);   // 内层 pResource 也要显式释放

// ② 每次 CopyTextureRegion 之后（copy_rect 助手内）：
std::mem::ManuallyDrop::drop(&mut dst.pResource);
std::mem::ManuallyDrop::drop(&mut src.pResource);
```

**已落地代码里的泄漏点（逐文件，P09 修）**：

| 文件 | 行 | 泄漏对象 | 每帧次数 |
|---|---|---|---|
| `render/windows/compositor.rs` | 715-722 `barrier` | back buffer（`PRESENT↔COPY_DEST`）×2 + persist ×1 | 3 |
| `render/windows/compositor.rs` | 675-689 `copy_rect` 的 `destination_location`/`source_location` | back buffer + persist | 2 |
| `render/windows/atlas.rs` | 254-262 `barrier`、229-240 copy locations | 图集纹理 + staging（**不是** back buffer，但同样泄漏） | 每脏行 |
| `render/windows/readback.rs` | 94-105 copy locations、238-245 `barrier` | readback 缓冲 + 源纹理 | 每次快照 |
| `render/windows/pipeline.rs` | 422 `pRootSignature` | root signature | 每个 PSO（一次性） |

> 只有前两行会影响 `ResizeBuffers`；其余三处是同一类 bug 的其它实例（长跑会持续泄漏 COM 对象），P09
> 一并修，避免"修了 resize 又留下别的钉住点"。

**resize 的调用顺序（probe 验证）**：
`queue.wait_idle()` → 释放全部 back buffer 引用（P08 不建 RTV，只有 `GetBuffer` 临时引用）→
`allocator.Reset()` + `list.Reset(allocator, None)`（**丢弃命令列表记录的 back buffer 引用**）→
`ResizeBuffers(0, w, h, DXGI_FORMAT_UNKNOWN, 0)` → `fresh = true`。
同尺寸 no-op（`w=h=0`）与变尺寸都 `S_OK`，可重复；失败仍一律 `EngineError::Swapchain`、**不重试**。

### 2.8 `windows 0.62.2` API 事实（编译期逐条核对，P08 §2.9 的补充）

| 事实 | 影响 |
|---|---|
| **不存在自由函数 `SetFocus` / `GetFocus`**（只有 `GetActiveWindow`、`SetForegroundWindow`） | 引擎**不能**以编程方式夺焦点；焦点只由 OS 的 `WM_SETFOCUS`/`WM_KILLFOCUS` 驱动 |
| `LoadKeyboardLayoutW` **不存在**（只有 `LoadKeyboardLayoutA`） | 需要换布局做测试时用 A 变体 + `s!("00000407")` |
| `SendMessageW(hwnd, msg, Option<WPARAM>, Option<LPARAM>)` | 传 `None` 而不是 `0`；`WM_DPICHANGED` 的 `lParam` 只能走它（同步） |
| `CreateSwapChainForHwnd` **5 参**、返回 `Result<IDXGISwapChain1>`（无 out 参数） | P08 的 `for_hwnd` 签名不变，内部调用点要改 |
| `CF_UNICODETEXT` 在 `Win32::System::Ole`，类型 `CLIPBOARD_FORMAT(u16)` | 传 `SetClipboardData` 要 `CF_UNICODETEXT.0 as u32` |
| `PostMessageW` / `SetClipboardData` / `OpenClipboard` 返回 `Result`（`BOOL`+`GetLastError`） | 错误必须 `hr_text(hr)` + `GetLastError` 双写，否则分不清「配额不足」与「参数错」 |
| `WM_PAINT` 的 `BeginPaint`/`EndPaint`/`InvalidateRect` 在 `Win32::Graphics::Gdi`（不是 `WindowsAndMessaging`） | 模块划分按实测，不按直觉 |
| `GlobalAlloc` 在 `Win32::System::Memory`；`GlobalFree` 在 `Foundation` | 同上 |

### 2.9 不可行 / 受限项与最小复现

| 项 | 最小复现 | 替代 |
|---|---|---|
| 真实 IME 输入（候选窗、preedit 光标） | 本机有 `HIMC` 但无输入法实际输入；`WM_IME_*` 只能合成 | 只实现消息通路 + `ImmSetCompositionWindow`；语义留 P12 实机（V-61） |
| dead key 的 `WM_CHAR` 组合 | 只能验证 `ToUnicodeEx` 的 -1/组合语义；无法用真实键盘产生死键消息 | 走 `TranslateMessage`+`WM_CHAR`（OS 组合），单测钉死 `WM_CHAR` 的消费路径 |
| 多 swapchain / 同 HWND 重建 | 泄漏引用时 `E_ACCESSDENIED`；修复后未再验证「重建」路线 | P09 选 `ResizeBuffers`（已闭环），不引入第二条 resize 路径 |
| 无桌面 CI 的 present | 无交互桌面时只能看 `Present1` 返回值，不能看像素 | demo 的 `--snapshot`（WARP 离屏）做像素断言；present 人工验收（V-62） |
| 真实 DPI 变化（跨屏拖动） | 无第二显示器/无交互桌面 | 合成 `WM_DPICHANGED` 走通换算；实机由 P12 的 L5 矩阵补 |

---

## 3. 模块结构与边界

### 3.1 文件树与非 test 消费者（dead_code 审计表）

`platform.rs`（模块根，**只**允许 `mod` / `pub(crate) mod` / `#[cfg] use … as …`；lint `definition_in_module_root`）：

```rust
//! 平台层（crate 私有；应用层不可见，I9）。
pub(crate) mod dispatcher;
pub(crate) mod event;

#[cfg(windows)]
pub(crate) mod windows;
#[cfg(windows)]
pub(crate) use windows as native;
```

| item | 定义 | 非 test 消费者 |
|---|---|---|
| `Dispatcher` / `FrameWake` / `UserEvent` | `platform/dispatcher.rs` | `event_loop.rs`（泵循环）、`AppHandle` |
| `PlatformEvent` / `Modifiers`(平台侧) / `translate` | `platform/event.rs` | `event_loop.rs::wndproc` |
| `NativeWindow`（`hwnd`/`dpi`/`min_size`/`visible`） | `platform/windows/window.rs` | `event_loop.rs`、`app.rs::attach_native` |
| `WndProcState`（按键 modifier 累积 / dead key pending / IME 组合） | `platform/windows/event_loop.rs` | 同文件 `wndproc` |
| `Dpi`（`query` / `scale_from_wparam` / `client_to_window`） | `platform/windows/dpi.rs` | `window.rs`、`event_loop.rs` |
| `ImeState`（`handle_message`） | `platform/windows/ime.rs` | `event_loop.rs` |
| `Clipboard`（`set_text` / `get_text`） | `platform/windows/clipboard.rs` | `event_loop.rs`（`WM_COMMAND`/快捷键留 P12）＋ 单测 |
| `Application` / `AppHandle` | `app.rs`（公开） | `crates/kytrix_gui_demo` |
| `Event` / `InputEvent` / `Key` / `Modifiers` / `KeyState` / `ImeEvent` / `PointerButton` | `event.rs`（公开） | `app.rs::dispatch`、`demo`、`test_support::app::inject` |

**可见性纪律**：`platform/` 整体私有（`lib.rs` 里 `mod platform;`）；`platform/windows/**` 内部项不进上表，同模块消费即可；
新增的每个 `pub(crate)` item 必须在上表里有一个非 test 消费者，否则 `cargo clippy --all-targets -D warnings` 的 `dead_code` 会拒。

### 3.2 无 trait / 无枚举分派（I1 / I14）

- 没有 `trait PlatformBackend`、没有 `Box<dyn EventLoop>`；`platform.rs` 用
  `#[cfg(windows)] pub(crate) use windows as native;` 做**编译期**选择。
- 唯一的 trait object 是 `Dispatcher` 的**唤醒回调**（`Arc<dyn Fn() -> bool + Send + Sync>`）：
  它的存在只为让「合并逻辑」可在没有 `HWND` 的单测里断言（一个 `AtomicUsize` 计数即可），
  不是后端抽象，也不参与平台选择。
- `event.rs`（公开）不依赖任何平台类型；`platform/event.rs` 的 `translate` 也不依赖 —— 它收
  `(u32, usize, isize, &mut WndProcState)`，Win32 常量只在调用点（`wndproc`）出现。

### 3.3 与既有代码的接线（签名变更，逐字）

```rust
// app.rs（公开面新增）
pub struct Application;
impl Application {
    /// 引擎自有消息循环：DPI 感知 → 建窗口（隐藏）→ 挂渲染器 → 显示 → 泵 → shutdown。
    ///
    /// 窗口/树在调用**之前**用 `App::create_window` + `App::on_frame` 准备好（`Run(app)` 而非
    /// `run(build)`：`build` 闭包拿不到 `&mut App` 与 `Application` 的先后关系，且 demo 需要
    /// 先建树再进循环）。
    pub fn run(self, app: App) -> Result<(), EngineError>;
}

#[derive(Clone)]
pub struct AppHandle { /* Arc<Dispatcher> */ }
impl AppHandle {
    /// 请求一帧（合并；任意线程可调）。
    pub fn wake(&self);
    /// 回投一个「在主线程执行」的闭包（业务任务结果的唯一入口）。
    pub fn send(&self, task: impl FnOnce(&mut App) + Send + 'static);
    /// 经 `App` 的 tokio Handle 提交任务（主线程内唯一允许的 spawn 入口）。
    pub fn spawn<F: std::future::Future<Output = ()> + Send + 'static>(&self, future: F);
}

impl App {
    pub fn handle(&self) -> AppHandle;                        // 纯新增（P09 前只有 runtime_handle）
    pub fn drain_events(&mut self) -> Vec<(WindowId, Event)>;  // 本帧平台事件
    pub fn on_frame(&mut self, hook: impl FnMut(&mut App) + 'static); // 每轮循环一次（读事件/写状态）
    pub fn set_clipboard_text(&self, text: &str) -> Result<(), EngineError>;
    pub fn clipboard_text(&self) -> Result<SmolStr, EngineError>;
}
```

`Window::frame()` 与 `App::frame(window)` 的签名、`FrameOutcome`、`FrameScheduler` **一律不变**；
`App::create_window` 仍只建 headless 窗口（P06/TestApp 行为零变化），原生窗口在 `Application::run` 内附挂。

---

## 4. 事件映射（定死）

### 4.1 三段式

```text
WndProc(msg, wparam, lparam)
  └─ platform::event::translate(...) -> Option<PlatformEvent>     // 纯函数、无 Win32 类型、可单测
       └─ App::dispatch(window, PlatformEvent)
            └─ Event::Input(InputEvent{ position: 逻辑像素 }) / Event::Ime / Dpi / Appearance / Focus / Resize / CloseRequested
```

`PlatformEvent`（私有）只有 8 个变体：`Pointer { position_px: (i32,i32), kind: PointerKind }`、
`Scroll { position_px, dx_ticks: i16, dy_ticks: i16 }`、`Key { vk: u16, scan: u32, state: KeyState, modifiers: Modifiers }`、
`Char(char)`、`Resize { width_px: u32, height_px: u32 }`、`Dpi { dpi: u32, suggested: Option<PhysicalRect> }`、
`Focus(bool)`、`Close`。

### 4.2 逐消息表（`translate` 的实现规格）

| `WM_*` | 解码（逐字） | 产出 |
|---|---|---|
| `WM_MOUSEMOVE` | `x = LOWORD(l)`、`y = HIWORD(l)`，两者按 `i16` 解（负坐标合法） | `Pointer::Move` |
| `WM_LBUTTONDOWN/UP`、`WM_RBUTTON…`、`WM_MBUTTON…` | 位置同上；按钮由 **msg** 决定，**不**读 `wParam` 的 `MK_*` | `Pointer::Down/Up(button)` |
| `WM_MOUSEWHEEL` | `delta = HIWORD(w) as i16`；位置在 `lParam` 是**屏幕坐标** → `ScreenToClient`（在 `wndproc` 内做，`translate` 只收客户端坐标） | `Scroll { dy_ticks: delta/120, dx=0 }` |
| `WM_MOUSEHWHEEL` | 同上；`dx_ticks = delta/120` | `Scroll { dx_ticks, dy=0 }` |
| `WM_XBUTTONDOWN/UP` | `GET_XBUTTON_WPARAM(w)` 1/2 → `X1/X2` | `Pointer::Down/Up(X1|X2)` |
| `WM_KEYDOWN` / `WM_SYSKEYDOWN` | `vk = w & 0xFF`、`scan = (l >> 16) & 0xFF`、`repeat = (l & (1<<30)) != 0`；`modifiers` 从 `WndProcState` 的累积状态取（`WM_KEYUP` 清） | `Key { state: Down, … }` |
| `WM_KEYUP` / `WM_SYSKEYUP` | 同上 | `Key { state: Up }` |
| `WM_CHAR` | `char::from_u32(w & 0xFFFF)`；代理对由两次 `WM_CHAR` 组成，`WndProcState` 拼（低代理缓存 1 个） | `Char(c)` |
| `WM_SETFOCUS` / `WM_KILLFOCUS` | — | `Focus(true/false)` |
| `WM_SIZE` | `width = LOWORD(l)`、`height = HIWORD(l)`（**客户端物理像素**）；`wParam` 是 `SIZE_*`（`SIZE_MINIMIZED=1` 时**丢弃**） | `Resize` |
| `WM_DPICHANGED` | `dpi = LOWORD(w)`（`HIWORD` 是 y-dpi，v1 要求两者相等，否则 `InvalidDpiScale`）；`suggested = *(lParam as *const RECT)` | `Dpi` |
| `WM_NCCALCSIZE` | `wParam==0` → 交 `DefWindowProcW`；`wParam==1` → 同样交 `DefWindowProcW`（P09 系统标题栏）；**返回值必须来自 `DefWindowProcW`** | 无事件 |
| `WM_GETMINMAXINFO` | 写 `ptMinTrackSize`（由 `NativeWindow.min_size` 的物理像素给出） | 无事件 |
| `WM_PAINT` | `BeginPaint` → `EndPaint`（**必做**） | 无事件（引擎自己 present，不靠 `WM_PAINT`） |
| `WM_TIMER` | `wParam == ANIMATION_TIMER_ID` → 推进动画（P13）；P09 不装定时器 | 无事件 |
| `WM_CLOSE` | `DestroyWindow` | `Close` |
| `WM_DESTROY` | `App` 里关窗；无窗口 → `PostQuitMessage(0)` | 内部 |
| `WM_IME_*` | 见 §4.4 | `Ime` |
| `WM_SETTINGCHANGE` | `lParam` 指向 `"ImmersiveColorSet"` 时读注册表 `AppsUseLightTheme` | `Appearance { dark }` |
| `WM_APP + 1`（`WM_APP_FRAME_WAKE`） | `wParam` 是 magic（debug 断言 `0x5EED`） | `FrameWake` |

**物理 → 逻辑（I11）**：`position = Point::new(scale.to_logical(x as f32), scale.to_logical(y as f32))`；
**唯一**反向换算点是 `Scale::to_logical`（`math.rs` 已有）。`WM_SIZE` 同理走 `Scale::to_logical` 后写
`Window::set_size`。滚轮的 tick → 逻辑滚动量由 P13 决定（P09 原样给 `ticks`）。

### 4.3 修饰键与 dead key

- `Modifiers` 由 `WndProcState` 维护：`WM_KEYDOWN` 的 `VK_SHIFT/CONTROL/MENU/LWIN/RWIN` 置位、`WM_KEYUP` 清位；
  **不**每帧调 `GetKeyState`（消息顺序即真值，probe §9 的合成消息也能正确跟踪）。
- **dead key**：P09 **不**自己处理 —— `TranslateMessage` 会先产生 `WM_DEADCHAR`（引擎忽略），随后
  `WM_CHAR` 给出组合后的字符（一条消息一个字符）。probe §14 验证了 `ToUnicodeEx` 的语义，但引擎**不**走它。
  若实机发现 `WM_CHAR` 组合缺失（V-61 的一部分），才把 `ToUnicodeEx` 作为 P12 的补充 —— 不为它预埋代码。

### 4.4 IME 最小集

| 阶段 | P09 行为 |
|---|---|
| `WM_IME_SETCONTEXT` | 原样交 `DefWindowProcW`（不做 `ISC_SHOWUICOMPOSITIONWINDOW` 抑制） |
| `WM_IME_STARTCOMPOSITION` | `ImeState.composing = true`；`Event::Ime(ImeEvent::Start)` |
| `WM_IME_COMPOSITION`（`lParam & GCS_COMPSTR`） | 用 `ImmGetCompositionStringW(himc, GCS_COMPSTR)` 取 preedit（`u16` → `String`，`SmolStr`）；`ImeEvent::Preedit` |
| `WM_IME_COMPOSITION`（`lParam & GCS_RESULTSTR`） | `ImeEvent::Commit(SmolStr)`；同时**不**再重复发 `WM_CHAR`（Windows 两者都可能给，去重靠 `ImeState.last_commit` 比较） |
| `WM_IME_ENDCOMPOSITION` | `composing=false`；`ImeEvent::End` |
| 任意时刻 | `ImmSetCompositionWindow(himc, COMPOSITIONFORM{ dwStyle: CFS_POINT, ptCurrentPos: 光标物理点 })`（probe 已验证调用成功） |

`ImmGetContext`/`ImmReleaseContext` **按消息成对**取用（不缓存 `HIMC`：P12 的 TSF 会改所有权）；
`himc` 无效时整条 IME 路径降级为「忽略」，但 `WM_CHAR` 仍然工作（可用的最小输入）。

---

## 5. 调度

### 5.1 `Dispatcher`（`platform/dispatcher.rs`）

```rust
pub(crate) struct Dispatcher {
    /// 唤醒投递（生产 = `PostMessageW`；单测 = 计数器）。返回 `false` = post 失败（配额）。
    post: Arc<dyn Fn() -> bool + Send + Sync>,
    /// 合并位：`true` = 已有一帧在队列里/正在处理。
    frame_pending: AtomicBool,
    /// tokio / a11y 回投的用户闭包（FIFO；单锁，非热路径）。
    tasks: Mutex<VecDeque<UserEvent>>,
    /// attach 之前 `0`。
    hwnd: AtomicIsize,
}

impl Dispatcher {
    /// 任意线程。**先置位再 post**：post 失败也保证主线程的下一次 drain 看到 pending。
    pub(crate) fn wake(&self) {
        if !self.frame_pending.swap(true, Ordering::AcqRel) {
            let _ = (self.post)();           // 失败不 panic：主线程 drain-then-check 会兜住
        }
    }
    pub(crate) fn take_frame_wake(&self) -> bool { self.frame_pending.swap(false, Ordering::AcqRel) }
    pub(crate) fn push(&self, task: UserEvent);            // 先入队再 wake
    pub(crate) fn drain_tasks(&self) -> Vec<UserEvent>;    // 主线程
    pub(crate) fn attach(&self, hwnd: isize);
}
```

**为什么必须合并**（probe §7）：`PostMessageW` 在 10,000 条后返回 `0x80070718`；未合并时每一次
`App::write` / 每个 tokio 结果都会 post 一条，高频行情下几秒就满。合并后**至多 1 条**在队列。

### 5.2 泵循环（`windows/event_loop.rs`）

```text
loop {
    // ① 先 drain（PeekMessageW 循环，PM_REMOVE）：WM_QUIT 显式 break
    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) {
        if msg.message == WM_QUIT { state.quit = true; break }
        TranslateMessage(&msg); DispatchMessageW(&msg);
    }
    if state.quit { break }

    // ② 处理唤醒/用户任务（dispatcher 的 pending 位已在 dispatch 时置位）
    if dispatcher.take_frame_wake() { frame_all(&mut app)? }
    for task in dispatcher.drain_tasks() { task(&mut app) }
    if app.no_windows() { break }

    // ③ 阻塞等待（I3）：没有消息时停在这里
    if GetMessageW(&mut msg, None, 0, 0) == 0 { break }   // WM_QUIT
    TranslateMessage(&msg); DispatchMessageW(&msg);
}
```

- `wndproc` 只在栈上写事件（`Vec<PlatformEvent>` 到 `WndProcState`），**不做**任何 `App` 调用 ——
  `wndproc` 里的 `App` 指针会与 Rust 借用规则冲突；泵循环在 `DispatchMessageW` 返回后统一消费。
- `WM_PAINT` 在 `wndproc` 里就被验证（`BeginPaint`/`EndPaint`），事件队列里不留痕。

### 5.3 帧驱动

| 何时 | 谁 | 说明 |
|---|---|---|
| 平台事件导致 dirty | `wndproc` 末尾 `dispatcher.wake()` | 输入/DPI/resize/焦点都会唤醒；「只在 dirty 后」由 **`App::frame` 的两段门禁**兜住（`needs_frame` 为假 ⇒ `FrameOutcome::Idle`，不提交、`FrameIndex` 不变） |
| 应用写状态 | `App::write` → `wake_requested` | `App::frame` 读 `take_wake_request()` |
| 跨线程结果 | `AppHandle::{wake,send,spawn}` | 合并 + 任务队列 |
| 首帧 | `Application::run` 显示窗口后 `handle.wake()` | 保证首帧一定画 |
| 动画 | P13 的 `SetTimer` | P09 无 |

**`Window::frame` vs `App::frame` 的调用点**：泵循环**只**调 `App::frame(window)`（生产路径必须跑
styles/text pass）；`Window::frame` 保持只给无绑定的测试/嵌入方使用（其 `debug_assert` 不变）。

### 5.4 `Application::run` 生命周期（逐字）

```text
Application::run(build):
 1. platform_init:
    a. SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)；失败（已是其它 awareness）→ 继续但记录
    b. RegisterClassW{ lpfnWndProc: wndproc, hbrBackground: null }（幂等；ERROR_CLASS_ALREADY_EXISTS 视为成功）
    c. Dispatcher 建立（post 回调 = PostMessageW(WM_APP+1)；hwnd 尚未 attach）
 2. let mut app = App::new(handle_from_caller_runtime)   // demo: main 建 runtime → App::new(handle)
    build(&mut app)
 3. for (id, window) in app.windows() 按创建序:
    a. Renderer 需要先知道 HWND：CreateWindowExW（**隐藏**）→ 得到 HWND
    b. GetDpiForWindow → scale；AdjustWindowRectExForDpi(client = size×scale) → SetWindowPos
    c. Renderer::new_for_hwnd(hwnd, physical_size)  // 构建 device/queue/pipeline/atlas/swapchain
    d. Window::set_renderer(renderer)；window.set_scale(scale)
 4. 全部窗口 ShowWindow(SW_SHOWNORMAL) + Dispatcher::attach(第一个窗口的 HWND)
    （`Dispatcher` 只有一个唤醒目标：窗口销毁时改指仍存活的窗口；一个都不剩就直接退出泵，
     不再依赖 post —— 见 §5.1 的"先置位再 post"）
 5. Dispatcher::wake()  →  泵循环（§5.2）
 6. shutdown：Renderer 的 `queue.wait_idle()`（丢全部 in-flight）→ drop Renderer（COM 引用按 §2.7 的
    规则释放）→ DestroyWindow 全部 → UnregisterClassW → 返回 Ok(())
```

错误路径：任一步 `Err` ⇒ 先关掉已建的窗口与设备，再原样返回；**不静默降级**（I10）。
D8 检测在 3c 内部（`Renderer::new_for_hwnd` → `Device::new` → `GpuCaps::check`），
所以「无 GPU」在**窗口已建但未显示**时失败 ⇒ 用户看不到半成品窗口（§1.5 的时序）。

---

## 6. 窗口

### 6.1 创建参数（定死）

| 项 | 值 |
|---|---|
| class | `KytrixGuiWindow`（`hInstance = GetModuleHandleW(None)`；`hbrBackground = null`：**引擎自己画**，不擦背景） |
| style | `WS_OVERLAPPEDWINDOW`（`WS_CAPTION|WS_SYSMENU|WS_THICKFRAME|WS_MINIMIZEBOX|WS_MAXIMIZEBOX`）；**先不带** `WS_VISIBLE` |
| size | `SetWindowPos` 到 `AdjustWindowRectExForDpi(client = logical×scale, dpi)` 的窗口尺寸 |
| owner | 无（`None`）；v1 单窗口 |
| 标题 | `WindowOptions.title`（`SmolStr` → `Vec<u16>`，截断到 1024 u16） |
| 最小尺寸 | `NativeWindow.min_size = (320, 200)` 逻辑像素（`WM_GETMINMAXINFO` 换算成物理） |

### 6.2 标题栏 / `WM_NCCALCSIZE`

- P09 **保留系统标题栏**：`WM_NCCALCSIZE`（`wParam` 0 与 1 两种）一律 `DefWindowProcW`，返回值原样返回。
  依据 probe §8：默认客户区 462×273（窗口 480×320）；自绘返回 0 后客户区 = 窗口矩形，两侧行为都已实测。
- 自绘标题栏（`app_owns_titlebar_drag` / `WindowControlArea`）留 P12；届时只需把 `WM_NCCALCSIZE`
  在 `NCCALC_CUSTOM` 模式下返回 0 + 自己处理 `WM_NCHITTEST`，`platform/windows/window.rs` 已预留分支点。

### 6.3 resize

```text
WM_SIZE(w, l):
  if wParam == SIZE_MINIMIZED: return                      // 最小化不重建
  physical = (LOWORD(l), HIWORD(l))
  logical  = scale.to_logical(每个分量)                     // I11
  window.set_size(logical)                                 // 标根 LAYOUT + full_damage_pending
  dispatcher.wake()                                        // 下一帧提交时 resize
下一帧 commit（window.rs::submit_gpu 既有逻辑）：
  fresh = renderer.surface() != physical || renderer.is_fresh()
  if fresh { renderer.resize(physical)? }                  // 顺序见 §2.7
```

- **VBO / 实例缓冲不重建**：`InstanceBuffer` 容量跨帧保留，只有 persist 纹理与 swapchain buffer 重建（主设计 §5.5 / §9.3）。
- `ResizeBuffers` 失败 ⇒ `EngineError::Swapchain`，**不重试**（P08 §2.7 的结论不变；现在失败概率已被 F-86 降到"真故障"）。
- 最小尺寸：`WM_GETMINMAXINFO` 拦住用户拖拽；`WM_SIZE` 里再 `clamp(320,200)` 一次（外部 `SetWindowPos` 会绕过
  `GetMinMaxInfo` 的部分路径，probe §8）。

### 6.4 DPI / 外观

- `WM_DPICHANGED`：`scale = dpi / 96.0` → `Scale::new(scale)`；`None` ⇒ `EngineError::InvalidDpiScale`（**唯一**生产者是这里）。
  随后 `SetWindowPos` 到 `lParam` 的建议 `RECT`（**必须**采用，否则用户拖到另一块屏时窗口尺寸会跳），
  再 `window.set_scale(scale)`（P06 已有：标根 `LAYOUT|PAINT` + `scale_epoch++` ⇒ 图集换代）。
- 外观：`WM_SETTINGCHANGE` 且 `lParam` 指向 `ImmersiveColorSet` ⇒ 读
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\AppsUseLightTheme`（`Win32_System_Registry` 已在 features）；
  写 `Event::Appearance { dark }`。P14 的 `ColorResolver` 接在它上面。

### 6.5 关闭 / 退出

| 触发 | 行为 |
|---|---|
| `WM_CLOSE` | `props.quit = true` 由泵处理：`App::close_window(id)` + `DestroyWindow`；若这是最后一个窗口 → `PostQuitMessage(0)` |
| `App::close_window`（应用层调用，如 Esc） | 关掉该窗口的 HWND（`DestroyWindow`）并移除；无窗口 → 泵退出 |
| `Application::run` 返回 | 正常 `Ok(())`；设备/COM 全部 `wait_idle` 后 drop |
| 致命错误（device lost / swapchain） | 立刻退出泵并返回 `Err`（不吞） |

---

## 7. demo（`crates/kytrix_gui_demo`）

### 7.1 结构

```text
crates/kytrix_gui_demo/
├── Cargo.toml          # [package] name="kytrix_gui_demo"; [[bin]] name="kytrix_gui_demo"
│                       # deps: kytrix_gui (workspace), tokio (workspace), image (workspace, png only)
│                       # features = { kytrix_gui = ["test-support"]（仅 --snapshot 需要） }
└── src/
    ├── main.rs         # 解析参数 → 建 tokio runtime → App::new(runtime.handle().clone())
    │                   #   → Application::run(|app| scene::build(app))；--snapshot 走另一条路
    └── scene.rs        # pub(crate) fn build(app: &mut App) -> (WindowId, Signal<Palette>)
                        #   节点：背景 quad + 3 个彩色 quad + 1 个文本；bind_style 读 Signal
```

- **workspace**：`members` 加 `"crates/kytrix_gui_demo"`；`default-members` **仍是** `["crates/kytrix"]`
  ⇒ `cargo build` / CI 的默认目标不编译 demo，`cargo run -p kytrix_gui_demo` 才编。
- 依赖声明只在 workspace 根（`image` / `tokio` 已在）；demo 的 `Cargo.toml` 用 `{ workspace = true }`。

### 7.2 `main`

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    match args.first().map(String::as_str) {
        Some("--snapshot") => snapshot(runtime, args.get(1)),   // WARP 离屏 + PNG
        _ => {
            // `runtime` 由 demo 拥有；引擎只拿 Handle（主线程不在 runtime 上下文，禁止 `tokio::spawn`）
            let mut app = kytrix_gui::app::App::new(runtime.handle().clone());
            let palette = scene::open(&mut app);               // 建窗 + 建树
            scene::install(&mut app, palette);                 // on_frame：点击改色 / Esc 关窗
            kytrix_gui::app::Application::new().run(app)?;
            Ok(())
        }
    }
}
```

`Application::default()` 收 `runtime.handle().clone()`（`Application::new(handle)`），
**demo 内没有任何 `tokio::spawn`**：`scene::install_input` 只读 `App::drain_events()`。

### 7.3 场景与预期像素

| 元素 | 逻辑矩形（`Rect::new(x,y,w,h)`） | 颜色（sRGB） | 说明 |
|---|---|---|---|
| 背景 | 0,0,800,600 | `#101418` | 根节点 `PaintSpec.background` |
| 红 | 40,40,200,120 | `#E5484D` | 命中 → 换成 `#F2555A` |
| 绿 | 280,40,200,120 | `#30A46C` | 命中 → 换成 `#3CB179` |
| 蓝 | 520,40,200,120 | `#0091FF` | 命中 → 换成 `#2B9FFF` |
| 文本 | 40,200,720,40 | `#E6E8EB` | `"Kytrix GUI · Phase 09 · click a swatch, Esc to quit"` |

- 窗口逻辑 `800×600`、`WindowOptions.scale = Scale::ONE`；DPI 由平台覆盖（本机 1.25 ⇒ 物理 1000×750）。
- `--snapshot` 用 `SnapshotCase { size: 800×600, scale: 1.0 }` ⇒ 像素断言与屏幕 DPI 无关。
- 预期像素（`SnapshotImage::rgba8`，**预乘 RGBA8**）：背景中心 `(400,500)` ≈ `#101418`；
  三个 quad 中心 `(140,100)` / `(380,100)` / `(620,100)` 分别为三色；文本带 `y∈[200,240]` 内非背景像素 > 0。
- **命中**：点击红块中心（逻辑 140,100）后，该处像素变成 `#F2555A`（下一次 `--snapshot` 可复现，
  或手动点击后肉眼确认）。

> **P15 扩展**：demo 已从本节的「三色块 + 一行文本」扩成 6 张卡片（① 焦点与动作 / ② 焦点状态 /
> ③ 中文排版 / ④ 无障碍 / ⑤ 输入法 / ⑥ 图片与滚动），窗口 `960×740`。本节保留 P09 的最小场景作为
> **历史基线**；现行结构与实测见 `design/phase-15-demo-showcase.md`。

### 7.4 运行与截图命令

```powershell
# 1) 真实窗口（人工验收：present / 输入 / DPI / resize / 关闭）
cargo run -p kytrix_gui_demo

# 2) 离屏像素（CI 可跑；WARP，不需要桌面）
cargo run -p kytrix_gui_demo --features snapshot -- --snapshot out\demo.png

# 3) 外部工具截屏（验收"屏幕上的确是这张图"）
#    PrintWindow(hwnd, hdc, PW_RENDERFULLCONTENT=2) → BMP；或 Win+Shift+S
```

`--snapshot` 的 PNG 是**引擎自己渲染的像素**（`TestApp::snapshot` → WARP offscreen → readback），
与窗口 present 路径共享 `Scene`/`InstanceBuffer`/PSO，唯一差别是目标纹理（P08 F-71 的 persist 即读回对象）。

---

## 8. 公开面 / 死代码 / 规范

### 8.1 公开面（P09 只新增这些）

| 路径 | 说明 |
|---|---|
| `kytrix_gui::app::{Application, AppHandle}` | `Application::new(handle)` / `run(build)`；`AppHandle::{wake,send,spawn}` |
| `kytrix_gui::app::App::{handle, drain_events}` | 纯新增 |
| `kytrix_gui::event::{Event, InputEvent, PointerButton, Key, KeyState, Modifiers, ImeEvent}` | 公开白名单内的类型；`test_support::input` 的同名定义**删除** |
| `kytrix_gui::error::EngineError::WindowCreate { detail: SmolStr }` | `CreateWindowExW` / `RegisterClassW` 失败的可断言表达（`AppInit` 留给设备/管线） |

**不新增**：`Window::hwnd()`（I9）、`Application::screenshot()`（截图属 test-support/demo）、
`App::on_event(closure)`（P10 的 `Action` 分发会重新定义这个面）。

### 8.2 私有与死代码

- `platform` 与 `render` 都是私有模块（`lib.rs` 里 `mod platform;` / `mod render;`）；`HWND`/`MSG`/`HIMC`
  只在 `platform/windows/**` 出现（I9）。
- 每个 `pub(crate)` 新项都必须有非 test 消费者（§3.1 表）；禁止 `#[allow(dead_code)]`、`#[allow(...)]`、
  `#[expect(...)]`（AGENTS §4）。probe 里用到的 `SetTimer`/`ToUnicodeEx` **不**进仓库代码（无消费者）；
  它们的事实写进本文与 DEVIATIONS。
- 无 `#[path]`、无 `mod.rs`（DEVIATIONS M-06：自命名模块根 `platform.rs` + `platform/`）。
- 每个 `unsafe` 块都带 `// SAFETY:`，逐条说明：指针来源（Win32 结构体/out 参数）、生命期
  （`MSG`/`PAINTSTRUCT` 在栈上、`lParam` 只在消息处理期有效）、线程（只在主线程调 Win32 UI）。
- 不新增 mac/linux 文件：P09 只有 `platform/windows/**`；`platform.rs` 用 `#[cfg(windows)]` 门控。
  **`Application` 在 `app.rs` 里也是 `#[cfg(windows)]`**（P11/P17 落地各自平台时解除），因此非 Windows 构建
  不存在 `Application`，也就不会有"假装能跑"的实现（I10 / AGENTS §1.1）。`AppHandle` 与 `Dispatcher` 是
  平台无关的（不碰 `HWND`），保持全平台编译，供 P11/P17 直接复用。见 §10 R-3。

### 8.3 demo crate 的规范

- `main.rs` 只做启动与参数；业务建树在 `scene.rs`（AGENTS §5.4）。
- 不复制引擎内部实现；只用公开 API（`App`/`NodeBuilder`/`Style`/`Signal`/`Application`）。
- `--snapshot` 用 `test_support` feature（demo 的 Cargo features 里显式打开），**不**把测试基建带进主二进制。

---

## 9. 测试

### 9.1 单元测试（全部无 GPU、无桌面）

| 模块 | 用例（可断言） |
|---|---|
| `platform/event.rs` | `WM_MOUSEMOVE` 负坐标（`0xFFFF_FFFE` → -2）；`WM_MOUSEWHEEL` 的 `+120/-120/0`；`WM_XBUTTON` 解出 X1/X2；`WM_SIZE` 的 `SIZE_MINIMIZED` 丢弃；`WM_DPICHANGED` `(192,192)` → scale 2.0 且不等时 `InvalidDpiScale`；`WM_CHAR` 代理对拼接；`WM_KEYDOWN` 的 repeat 位 |
| `platform/windows/dpi.rs` | `scale_from_dpi(96)=1.0` / `120=1.25` / `144=1.5` / `192=2.0`；`client_to_window` 用 96 与 120 的 frame 差（16×39 / 18×47） |
| `platform/dispatcher.rs` | 连续 `wake()` 3 次 → post 回调计数 **1**；`take_frame_wake()` 后再 `wake()` → 计数 2；`push` 顺序 FIFO；post 失败时 pending 仍为 true |
| `platform/windows/clipboard.rs` | `utf16_with_nul("")` = `[0]`；`utf16_with_nul("你好🌍")` 长度 = 4+2+1（含代理对）；读回时按 NUL 截断 |
| `platform/windows/ime.rs` | `GCS_COMPSTR` → `Preedit`；`GCS_RESULTSTR` → `Commit` 且 `last_commit` 去重；`WM_IME_ENDCOMPOSITION` 清态 |
| `event.rs` | `Event`/`InputEvent` 的 `PartialEq` 语义；`InputEvent::position()` 对 `Scroll` 返回指针位置 |
| `window.rs`（既有 + 新增） | `set_size` 后 `full_damage_pending`；`surface != physical` ⇒ `fresh`；`set_scale` 的 `scale_epoch` 递增 |
| `render/windows/queue.rs` | `reset_all_lists` 幂等；`wait_idle` + reset 之后 ring 的 slot 值不变 |

### 9.2 集成 / 手动

| 项 | 方式 |
|---|---|
| 真实窗口 / present / 输入 / resize / 关闭 | `cargo run -p kytrix_gui_demo` 人工跑一遍（截图存 `release_notes/` 或 PR） |
| 像素 | `cargo run -p kytrix_gui_demo --features snapshot -- --snapshot out\demo.png` + §7.3 的坐标断言（可写成 demo 的自检 `--check` 模式） |
| 无 GPU 错误路径 | `KYTRIX_GUI_FORCE_D8_FAIL=1`（P08 已有的 test-only 注入？——P08 用的是单测注入 `GpuCaps`）：P09 用**单测**断言 `Application::run` 在 `Renderer::new_for_hwnd` 返回 `UnsupportedGpu` 时窗口未显示且错误原样返回（把 `platform_init`/`attach` 拆成可测的纯函数，`run` 只做编排） |
| DPI | 合成 `WM_DPICHANGED`（`SendMessageW`）后断言 `window.scale()` 与 `GetClientRect` 的新尺寸 |
| 消息翻译 | 用 `SendMessageW(hwnd, WM_*, w, l)` 直接打（probe §9 已验证可行），断言 `App::drain_events()` 的内容 |

---

## 10. 风险与待复核

| # | 风险 | 处置 |
|---|---|---|
| R-1 | **`ResizeBuffers` 的剩余不确定性**：probe 只在本机（RTX 5080 / 非交互会话）闭环；其它驱动、多显示器、混合 DPI 下未验证 | 保留 P08 的「失败即 `Swapchain`、不重试」；登记 V-60（本轮已把根因写死）；P12 的 L5 矩阵补实机 |
| R-2 | **present 与消息泵的同步**：`Present1(sync=1)` 在泵线程内阻塞，会推迟消息处理（本机实测每次 present 几毫秒，可接受） | P09 保持同步提交；P13 再评估 `FRAME_LATENCY_WAITABLE_OBJECT`（本机创建该 flag 的 swapchain 直接失败，probe §2.7 变体 6） |
| R-3 | **非 Windows 构建**：`platform` 不编译 ⇒ `Application` 不存在，`kytrix_gui_demo` 无法在 CI 的 Linux/macOS 上编译 | demo 的 `[[bin]]` 加 `required-features`？更简单：demo 的 `main.rs` 用 `#[cfg(windows)]` 的 `fn main` + 非 Windows 的 `fn main(){ println!("windows only") }`；CI 的跨平台检查只 `cargo check -p kytrix_gui`（P08 已如此） |
| R-4 | **`WM_PAINT` 风暴**：漏一次 `EndPaint` 就 100 ms 内 2.4 万条消息，CPU 打满 | `wndproc` 的 `WM_PAINT` 分支只有 `BeginPaint`/`EndPaint` 两行；单测不覆盖（需要窗口），靠 demo 的人工跑 + code review |
| R-5 | **IME 复杂度**：组合串/候选窗/去重在真实输入法（微软拼音、日文）下有组合顺序差异 | P09 只做消息通路 + `ImmSetCompositionWindow`；`WM_CHAR` 兜底；P12 用真实输入法补（V-61） |
| R-6 | **demo 在无桌面 CI 上不能 present** | `--snapshot` 是 CI 路径；present 属人工验收（V-62），并在 `README`/PR 里写明 |
| R-7 | **`ManuallyDrop` 修复的回归风险**：修 `transition`/`copy_rect` 时若漏一处就回到 `0x887A0001` | 修完后必须跑 `hidden_window_swapchain_accepts_a_full_present` 的**取消 `#[ignore]`** 版本 + demo 的手动 resize；单测断言「同一 back buffer 的引用计数不增长」不可行，改为 demo 连点 resize 20 次不失败 |
| R-8 | **`WM_QUIT` 消费**（probe 首轮真事故） | 泵循环显式 `if msg.message == WM_QUIT { break }`；单测用 `PeekMessageW` 的伪造消息不可行，写进 `event_loop.rs` 的注释与 review checklist |

---

## 附录 A：probe 骨架（可编译核心，`%TEMP%\kg-p09-win32\src\main.rs` 的裁剪）

```rust
// 1) 窗口类 + 隐藏窗口
let instance = GetModuleHandleW(None)?;
let wc = WNDCLASSW { style: WNDCLASS_STYLES(0), lpfnWndProc: Some(wndproc),
    hInstance: instance.into(), hbrBackground: HBRUSH::default(),
    lpszClassName: PCWSTR(class.as_ptr()), ..Default::default() };
let atom = RegisterClassW(&wc);                       // 0 = 失败（GetLastError 区分 1410 已存在）
let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), PCWSTR(class.as_ptr()), PCWSTR(title.as_ptr()),
    WS_OVERLAPPEDWINDOW, 100, 100, 480, 320, None, None, Some(instance.into()), None)?;

// 2) 泵：先 drain（显式识别 WM_QUIT）再阻塞
loop {
    let mut msg = MSG::default();
    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
        if msg.message == WM_QUIT { break 'outer }
        let _ = TranslateMessage(&msg); DispatchMessageW(&msg);
    }
    if GetMessageW(&mut msg, None, 0, 0).0 == 0 { break }        // WM_QUIT
    let _ = TranslateMessage(&msg); DispatchMessageW(&msg);
}

// 3) 唤醒（合并）
if !pending.swap(true, Ordering::AcqRel) {
    let _ = PostMessageW(Some(hwnd), WM_APP + 1, WPARAM(0x5EED), LPARAM(0));
}

// 4) swapchain + 一帧（关键：两层 ManuallyDrop 都释放）
let chain: IDXGISwapChain1 = factory.CreateSwapChainForHwnd(&queue, hwnd, &desc, None, None::<&IDXGIOutput>)?;
let mut barrier = /* D3D12_RESOURCE_BARRIER transition back: PRESENT -> COPY_DEST */;
list.ResourceBarrier(std::slice::from_ref(&barrier));
{ let t: &mut D3D12_RESOURCE_TRANSITION_BARRIER = &mut barrier.Anonymous.Transition;
  ManuallyDrop::drop(&mut t.pResource); }                        // ← 少这一行 → ResizeBuffers 永远 0x887A0001
let mut dst = D3D12_TEXTURE_COPY_LOCATION { pResource: ManuallyDrop::new(Some(back.clone())), .. };
let mut src = D3D12_TEXTURE_COPY_LOCATION { pResource: ManuallyDrop::new(Some(persist.clone())), .. };
list.CopyTextureRegion(&dst, 0, 0, 0, &src, Some(&D3D12_BOX { left:0, top:0, front:0,
    right: w, bottom: h, back: 1 }));
ManuallyDrop::drop(&mut dst.pResource);
ManuallyDrop::drop(&mut src.pResource);
let hr = chain.Present1(1, DXGI_PRESENT(0), &DXGI_PRESENT_PARAMETERS::default());
```

## 附录 B：复现

```powershell
$env:CARGO_TARGET_DIR = "$env:TEMP\kg-p08-d3d12\target"   # 复用 P08 probe 的增量产物
cargo build --offline --manifest-path "$env:TEMP\kg-p09-win32\Cargo.toml"
& "$env:TEMP\kg-p08-d3d12\target\debug\kg-p09-win32.exe"          # 平台全量（≈15 步）
foreach ($i in 0..25) { & "$env:TEMP\kg-p08-d3d12\target\debug\kg-p09-resize.exe" $i }
& "$env:TEMP\kg-p08-d3d12\target\debug\kg-p09-resize2.exe"        # 修复验证（S_OK）
```
产物：`%TEMP%\kg-p09-win32\{probe-run.txt,probe-run2.txt}`（全量输出）、`src\main.rs`（平台 probe）、
`src\bin\kg-p09-resize.rs`（20 变体）、`src\bin\kg-p09-resize2.rs`（修复验证）。

## 附录 C：probe 输出摘录（`probe-run2.txt`）

```text
[P09] RegisterClassW ok：atom = 0xC649
[P09] GetWindowRect = (100,100)-(580,420)  480x320
[P09] GetClientRect = (0,0)-(462,273)  462x273
[P09] GetDpiForWindow = 120 → scale = 1.25（引擎 Scale 的输入）
[P09] AdjustWindowRectExForDpi(dpi=96, …) → window 496x359（frame 16x39）
[P09] AdjustWindowRectExForDpi(dpi=120, …) → window 498x367（frame 18x47）
[P09] 不调 BeginPaint/EndPaint 的 100ms 内 WM_PAINT 次数 = 24576（风暴）
[P09] SetTimer(16ms)：320ms 内 13 次 → 实测周期 ≈ 24.6ms
[P09] SetTimer(1ms)：320ms 内 20 次 → 实测周期 ≈ 16.0ms
[P09] GetMessageW 阻塞 162.9ms 后返回 WM_APP_FRAME_WAKE(0x8001) wParam=0x5EED；循环返回次数=1
[P09] 连续 PostMessageW 成功 10000 次后失败：0x80070718 `配额不足…` (GetLastError=1816)
[P09] 默认模式：window 480x320 / client 462x273
[P09] 自绘模式：window 802x602 / client 802x602（返回 0 后客户区应≈窗口）
[P09] SetWindowPos(120x90) 之后 window = 220x160
[P09] WM_MOUSEMOVE → 客户端坐标解码 = Some((123, 45))
[P09] 滚轮 delta 序列 = [120, -120, -120]
[P09] WM_DPICHANGED → dpi=(192,192) scale=(2,2)
[P09] ① 隐藏窗口 + 首帧全量 Present1 → 0x00000000 `操作成功完成。`
[P09] ② 显示后全量 Present1 → 0x00000000
[P09] ③ 局部 dirty(8,8)-(24,24) Present1 → 0x00000000
[P09] ④ present 之后 ResizeBuffers(0,0,0,UNKNOWN,0) → 0x887A0001   ← 修复前
[P09] 读回 = `Kytrix P09 clipboard 你好 🌍`（与写入相等 = true）
[P09] ImmGetContext = HIMC(0x73510ddd)（有 IME 上下文）
[P09] 德语布局 dead key 命中：VK_OEM_5=0xDC（ToUnicodeEx = -1）
[P09] 随后 ToUnicodeEx(VK_A) = 1 `â`
[P09] PostQuitMessage 之后 GetMessageW 返回 0

[P09R2] frame1: 拷贝帧 Present1 → 0x00000000
[P09R2] ★ ResizeBuffers → S_OK（ManuallyDrop 双层释放后 resize 恢复）
[P09R2] ★ 变尺寸 ResizeBuffers((624, 385)) → S_OK
```







---

## 11. 实施记录（2026-11-13，P09 落地）

| 项 | 实测 |
|---|---|
| 新增文件 | `src/event.rs`；`src/platform.rs` + `platform/{dispatcher,event}.rs` + `platform/windows/{event_loop,window,dpi,ime,clipboard}.rs`；`crates/kytrix_gui_demo/{Cargo.toml,src/main.rs,src/scene.rs}` |
| 公开面（实际） | `app::{Application, AppHandle}`（`Application::new()` / `run(app: App)`）、`App::{handle, on_frame, drain_events, set_clipboard_text, clipboard_text}`、`event::{Event, InputEvent, PointerButton, Key, KeyState, Modifiers, ImeEvent}`、`EngineError::{WindowCreate, Clipboard}`。**`run` 收 `App` 而非 `build` 闭包**：demo 需要先建树再进循环，闭包形态会逼出「窗口已建但钩子未装」的中间态 |
| F-86 修复 | `render/windows/{compositor,atlas,readback,pipeline}.rs` 的 `ManuallyDrop` 双层释放（barrier 的 union 载荷 + `pResource`、copy location 的 `pResource`、PSO 的 `pRootSignature`）；`Queue::reset_in_flight_lists`（`wait_idle` 之后、`ResizeBuffers` 之前）；`Renderer::resize` 补调 |
| 平台层 | 消息泵（`WM_QUIT` 显式识别、`WM_PAINT` 只验证、`WM_APP+1` 唤醒合并）、系统标题栏、`WM_GETMINMAXINFO` 最小尺寸、DPI（`GetDpiForWindow`/`AdjustWindowRectExForDpi`/`WM_DPICHANGED` 建议矩形）、鼠标/滚轮/键盘/字符/焦点、最小 IME（组合窗定位）+ 剪贴板（`CF_UNICODETEXT` 往返） |
| demo 实机 | `cargo run -p kytrix_gui_demo`：窗口标题 `Kytrix GUI · Phase 09`，客户区 1000×750（逻辑 800×600 @ scale 1.25），三个圆角色块 sRGB 正确，`PrintWindow` 截图 `%TEMP%\kytrix-demo.png`（1018×797，含系统标题栏） |
| demo 离屏 | `--features snapshot -- --snapshot out.png`（WARP + readback + `image` 编码）：800×600，色块像素与预期一致 |
| **V-64 已修** | 文本不可见的根因是 **P08 图集分块上传复用同一 staging**：一次上传按 256 行分块，CPU 逐块写同一缓冲并逐条录制 copy，GPU 执行时每条 copy 读到的都是**最后一块**的内容（2048² 页有 8 块，前 7 块写的是全 0）。64²/256² 的测试页只有 1 块，所以 P08 全绿。修复：staging 按本次上传跨度增长（`ensure_staging`）+ 每段只录一次 `CopyTextureRegion`；回归 `glyph_run_samples_the_uploaded_glyph_page`（64…2048 页尺寸扫描，修复前 ≥512 必失败）与 `rendered_text_produces_visible_glyph_pixels`（黑底白字覆盖率断言）。demo 截图文本可见 |
| 门禁 | `cargo test -p kytrix_gui` 491 passed / 1 ignored；`--features test-support` 500 + 5；`-p kytrix_gui_demo` 1；`clippy --all-targets --all-features -D warnings` rc=0；`fmt --check` rc=0；`cargo doc --no-deps` rc=0；`--release` build rc=0；`KYTRIX_GUI_SKIP_SHADERS=1 cargo check --target aarch64-apple-darwin` rc=0；`x86_64-unknown-linux-gnu` 需 stub `pkg-config`（P08 既有做法，`PKG_CONFIG=…\pkg-config.exe` + `PKG_CONFIG_ALLOW_CROSS=1`）rc=0 |
| 死代码 | `#[allow(` 0 处、`#[expect(` 0 处、`#[path]` 0 处；`platform` 全私有（I9）；无 mac/linux 平台文件 |
| 与设计的偏差 | 见 DEVIATIONS §14.3（F-92 新增 windows feature `Win32_System_Memory`；S-84 API 形态；S-85 P09 整页字形上传；V-64 文本像素缺陷） |


