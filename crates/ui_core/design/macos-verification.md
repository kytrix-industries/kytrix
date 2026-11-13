# macOS 平台验证记录（macOS 15.8.1 / Intel Iris Plus 655 / Metal 3）

> 目标：把 `ui_core` 在 Windows / Linux 上已完成的功能面逐项搬到 macOS 实机验证，发现并修复 BUG，
> 记录每个修复的用户决策与复现证据。
>
> 本文是**执行记录**；与设计文档不同的实现事实按纪律登记到
> [`DEVIATIONS.md`](DEVIATIONS.md)（本轮新增 F-157 … F-165、S-138、V-114 … V-119；
> V-114/V-119 已由 F-164 关闭，保留编号作为历史）。

---

## 0. 结论摘要

| 项 | 结论 |
|----|------|
| 验证前状态 | macOS 后端能编译、能出帧，但 IME 预编辑串被 NSAttributedString 的 `description` 污染；自持事件循环无法完成原生 fullscreen 转场；HUD 连续出帧时 AX 查询可卡住；macOS 13 个 golden 全部跳过；macOS 代码有 4 个 clippy error；`PointerLeave` 因缺少 `NSTrackingArea` 永不触发 |
| 修复后状态 | 编译、测试、release、clippy、fmt、doc、bench 全绿；真实窗口（Metal）出帧、交互、IME、a11y、剪贴板、DPI（2x↔1x）、resize、关窗实测通过；全屏 **HUD 开/关**均进出正常、无残留；HUD 60fps 下完整 AX 树 0.20s；`metal-real/` 新增 13 份像素基线 |
| 发现 BUG | **7 项**（1 项 IME 功能缺陷、1 项门禁、1 项 L3 覆盖缺口、1 项指针事件缺失、1 项全屏生命周期缺陷、1 项渲染阻塞、1 项 HUD 连续帧退出挂起）；另按用户批准完成 **macOS 事件循环 + 异步提交 + 窗口 render 线程** 三轮架构改造 |
| 修复方式 | 全部经用户逐项决策后实施（见 §3 决策表）；架构改造只改 macOS，Windows/Linux 运行时不变 |
| 像素 golden | 新增 `tests/golden/metal-real/` 13 份；与 `warp/` 的最大逐通道差 **1**（超容差 0），与 `lavapipe/` 的差与 Linux 软件光栅一致（§5） |
| 测试门禁 | 数值见 §4.1；三平台行为差异只在 GPU/字体/IME 环境 |
| 截图弹窗 | macOS 15 的 ScreenCaptureKit「绕过窗口选择器」提示只在 **root 进程**调用 `screencapture` 时出现；改为以 console 用户运行后不再弹出（§6.1） |

---

## 1. 验证环境

| 项 | 值 |
|----|-----|
| 机型 / CPU | MacBook Pro（2018），Intel Core i5-8279U @ 2.40GHz，x86_64 |
| OS | macOS 15.8.1（24H32） |
| GPU | Intel Iris Plus Graphics 655，Metal 3，无独立显卡 |
| 显示 | 内建 Retina 2560×1600；系统当前模式 1680×1050（点）@2.00x，物理 3360×2100 |
| Xcode / Metal | Xcode 16.4（16F6），`metal` 32023.620 |
| Rust | stable 1.98.0；nightly 1.101.0（clippy/rustfmt） |
| 输入法 | 系统 Pinyin - Simplified（Apple SCIM）与 SogouInput（第三方进程在场） |
| 会话 | Aqua 本地会话；DSH/agent 以 root 运行，但 GUI 操作以 console 用户 `chenbaiyu` 启动 demo |
| 截图 | `sudo -u chenbaiyu screencapture …`（用户身份，避免 macOS 15 屏幕捕获确认弹窗） |

---

## 2. 发现的问题

### 2.1 IME 预编辑串被 NSAttributedString 属性污染

真实拼音输入 `nihao` 时，输入框与状态行显示：

```text
ni'hao{
    NSUnderline = 2;
    NSUnderlineColor = "NSCalibratedWhiteColorSpace 0.17 1";
}
```

`NSTextInputClient.setMarkedText:` 收到的是 `NSAttributedString`；`platform/macos/view.rs::object_text`
对所有对象调用 `-[NSObject description]`，`NSAttributedString` 的 `description` 会连同属性字典一起输出。
`insertText:`（提交）收到的是 `NSString`，所以上屏文本 `你好` 正常，只有预编辑串被污染。

**影响**：组合串显示错误、状态行被多行内容覆盖；长文本还会溢出输入框（单行文本节点没有裁剪）。
**修复**：`object_text` 先按 `NSAttributedString` 取 `-string`，再按 `NSString` 取文本；未知对象才回退
`description` 作为诊断。新增单测构造带 `NSUnderline` 属性的 `NSMutableAttributedString`，断言只返回正文。

### 2.2 HUD 连续出帧时 AX 查询卡住（`nextDrawable` 无限阻塞）

HUD 以 60fps 连续出帧时，对 demo 做一次完整 AX 树遍历会超过 15s；`sample` 显示主线程长期停在
`CAMetalLayer nextDrawable` 的 semaphore，AX MIG 线程在等主线程。杀掉 AX 客户端后立刻恢复。

根因是 `Swapchain`：

1. `setAllowsNextDrawableTimeout(false)`：没有可用 drawable 时无限期阻塞；
2. `present` 放在 `waitUntilCompleted` **之后**：3 张 drawable 在整个同步等待期间一直被占用。

**修复**（用户选 A）：
- 在 command buffer `commit` 之前调用 `MTLCommandBuffer::presentDrawable:`（Apple 推荐生命周期）；
- `allowsNextDrawableTimeout(true)` 作为安全网；`nextDrawable` 返回 `None` 时按「本帧跳过」处理。

**后续架构改造（用户批准）**：macOS 窗口路径先改为异步提交（`Queue::submit` 只 commit，完成后轮询，
§2.8），随后把整个 `GpuRenderer` 移到独立 render 线程（F-164，§2.9）：`nextDrawable`、编码、
`presentDrawable` 都不再经过主线程。HUD/动画 60fps 时完整 AX 树 0.20s；V-114/V-119 关闭。
命令提交仍异步，GPU 时间采样从上一帧读取（HUD 显示可能领先一帧）。

### 2.3 macOS clippy 门禁 4 个 error

```
platform/macos/event_loop.rs:190  collapsible_if
platform/macos/event_loop.rs:261  collapsible_if
platform/macos/event_loop.rs:287  collapsible_if
render/macos/readback.rs:107      chunks_exact_mut -> as_chunks_mut
```

macOS 后端此前只做交叉 `cargo check`，没有在 macOS 上跑过 `-D warnings`。按 clippy 建议修复：
event_loop 三处改 let-chain，readback 改 `as_chunks_mut::<4>().0`。不引入 `#[allow]`。

### 2.4 macOS 像素 golden 全部跳过（L3 覆盖缺口）

`skip_reason()` 在 macOS 返回「无 metal-real/ 基线」；13 个 L3 case 全部打印 skip，离屏 Metal
其实可用（`snapshot_demo` 已能出 PNG）。修复：删除 `skip_reason()` 与测试里的 skip 分支，
启用 macOS golden，更新 `device_dir()` 的测试断言/README/Cargo 注释，并生成 13 份 `metal-real/` 基线。

### 2.5 `PointerLeave` 永不触发（缺 NSTrackingArea）

`GuiView` 实现了 `mouseExited:`，但从未创建 `NSTrackingArea`。`NSWindow.setAcceptsMouseMovedEvents(true)`
只保证 `mouseMoved:`；`mouseExited:` 必须由 tracking area 产生。设计 §7.2 明确要求 tracking area，
实现遗漏。修复：在 `GuiView::new` 装一个 `InVisibleRect | ActiveInKeyWindow | MouseEnteredAndExited`
的 tracking area；**不要**同时声明 `.MouseMoved`（窗口已经投递 `mouseMoved:`，两者同时开启会让每个移动
产生两次 `PointerMove`——用临时 event probe 实机复现并修正）。需要给 objc2-app-kit 增加
`NSTrackingArea` feature。

### 2.6 全屏：事件循环 + 窗口属性 + 异步提交的组合修复

初始复现（自持 `nextEventMatchingMask` 循环）：AXFullScreenButton 进入全屏后无法完成转场——WindowServer
只剩一个 0,0 1680×1050 的 **AXUnknown** 窗口，内容是旧 960×740 快照；无 AXCloseButton/AXFullScreenButton，
Esc 与 Ctrl+Cmd+F 都不能退出，只能杀进程。直接写 `AXFullScreen` 属性退出后还会留下一个残影窗口。

根因不是 GPU 拉伸，而是 **pump 在 AppKit 的 fullscreen/live-resize run loop mode 中被饿死**，以及窗口
没有声明 fullscreen primary / contentView 不随窗口 autoresize / 窗口透明。按用户批准做了 macOS-only 改造：

1. 主循环换 `NSApplication::run()`，pump 由 `CFRunLoopObserver` + wake source + timer 驱动，全部挂 common modes；
2. `contentView` 设置 `ViewWidthSizable|ViewHeightSizable`；
3. 窗口 `setOpaque(true)` + 黑色背景，并显式 `collectionBehavior = FullScreenPrimary`；
4. 命令提交异步化（queue 轮询完成，不阻塞主线程）。

结果：**HUD 关闭**时 AXFullScreenButton 进出全屏正常（1680×1050 → 恢复 960×740，AX 树为 AXStandardWindow
且控件/子节点齐全，无残留）；demo 固定 960×740 布局，全屏四周黑底属预期。**HUD 开启**（连续 60fps）时
最初仍会停在旧快照，根因是 `nextDrawable` 在主线程；F-164（§2.9）把 drawable/编码移到 render 线程后，
HUD 开启的全屏也正常（1680×1050，AX 树完整，58–60fps）。

### 2.7 IME 提交后焦点/按键（观察项，未复现）

早期测试中，Sogou 候选窗按 Space 上屏后，后续按键不再进入 demo，直到点击输入框；引擎焦点面板仍显示
输入框，`AXFocusedUIElement` 变空。后续在**干净 Apple Pinyin 会话**复测：上屏「你好」后直接输入
`shi` 能继续出现 `组合中：shi`，按键没有丢失。该现象更像候选窗/系统弹窗抢走 key window 的夹具问题，
不是引擎稳定缺陷；记录为 V-116，未改引擎。

---

### 2.8 macOS 事件循环 + 异步提交架构改造（用户批准）

| 项 | 旧 | 新 | Windows/Linux 影响 |
|----|----|----|---------------------|
| 事件循环 | 引擎自持 `nextEventMatchingMask` + source 只 default mode | `NSApplication::run()`；observer/source/timer 全挂 common modes；退出先撤 source/timer，再投空事件 + `NSApp.stop`（F-165） | 无（只改 `platform/macos/event_loop.rs` / `wake.rs`） |
| 命令提交 | `commit` + `waitUntilCompleted` | `commit` 后立即返回；3 槽 in-flight 轮询 `status`，完成后 resolve timer + retire | 无（只改 `render/macos/queue.rs` / `compositor.rs`） |
| GPU 读回 | 同步等待 | `wait_idle` 只用于 resize/readback/shutdown；离屏 golden 仍同步 | 无 |
| 窗口属性 | 透明、无 collectionBehavior、contentView 无 autoresizing | opaque + FullScreenPrimary + autoresizing | 无 |

公共 `App::frame` / `Window::frame` / `TestApp` / FrameRecord / golden 语义不变；Windows/Linux 继续同步后端。
随后 F-164（§2.9）把窗口路径的 `GpuRenderer` 整体移到 render 线程，`nextDrawable` 不再出现在主线程；
V-114 / V-119 关闭。

### 2.9 窗口 render 线程（F-164）

**目标**：主线程不再调用 `CAMetalLayer::nextDrawable`，也不在编码/呈现上被合成器暂停拖住。

**结构**：

- `render/macos/renderer.rs` 的 `Renderer` 是对外门面：
  - `Offscreen(GpuRenderer)`：测试/快照，保持同步；
  - `Window(WindowRenderer)`：主线程句柄。
- 建窗时主线程创建 `GpuRenderer` + `CAMetalLayer` 并挂到 `NSView`，随后把 `GpuRenderer` 整体
  移交给 `kytrix-metal` 线程；`GpuRenderer` 内所有 Metal 对象此后只由该线程访问（`unsafe impl Send`
  的 safety 注释即这条独占约束）。
- 主线程每帧只构造 `FramePayload`（实例字节、`FrameOp` 绘制序、`GpuClipEntry` 裁剪表）并投进
  **最新帧邮箱**。图集上传走同一邮箱的控制队列，顺序与主线程调用一致。
- worker 独占 `nextDrawable`、命令编码、`presentDrawable`、`Queue` 完成轮询、resize/DPI 与
  `wait_idle`。主线程不再写 `CAMetalLayer.contentsScale` / `drawableSize`。
- **被替换的帧强制全量**：worker 忙时新帧覆盖旧帧，旧帧没有画进 persist，替代帧必须整面重画；
  **拿不到 drawable** 时 job 放回邮箱并强制全量，100ms 后重试，避免合成器暂停恢复后 damage 丢失。
- 主线程通过 `Window::poll_renderer` / `App::poll_renderers` 收 worker 的 stats/GPU 采样/错误；
  空闲泵也会轮询，设备丢失不会等到下一次输入才冒泡。HUD 的分段计时回退到 worker 最近完成帧。

**验证**：

- HUD 开启全屏：`AXFullScreenButton` 进入 1680×1050，HUD 58–60fps；完整 AX 树（74 行，depth=8，含菜单）
  **0.20s**；退出恢复 960×740，无残留。
- `sample` 的 `nextDrawable` 只出现在 `Thread_…: kytrix-metal`；主线程在 run loop / 帧逻辑，
  不再出现 `CAMetalLayer nextDrawable`。
- HUD 关闭静止：窗口区域两次截图逐字节相同，CPU 0.0%，`sample` 无 `frame_all`/`nextDrawable`。
- resize 1200×771 物理 2400×1542 @2x、DPI 2x↔1x 均正确；离屏 8 个 snapshot 测试与 13 份
  `metal-real` golden 不变。
- Windows/Linux 运行时路径不变；只有 macOS 的窗口路径改变。

### 2.10 HUD 连续帧下退出挂起（F-165）

HUD 开启时用 AXCloseButton 关窗：日志确认泵已处理 `CloseRequested`、native/app 窗口都为空并设置
`exit`，但进程不退出。根因是 60Hz `CFRunLoopTimer` 继续唤醒 run loop，而 `NSApp.stop` 之后
`NSApplication::run` 仍睡在 `nextEventMatchingMask` 等 NSEvent；`CFRunLoopWakeUp` 不产生 NSEvent。

修复：退出路径先 `wake::uninstall()` 撤掉 source/timer，再 `postEvent_atStart` 一个
`NSEventType::ApplicationDefined` 空事件，最后 `NSApp.stop(None)` + `CFRunLoop::stop`/`wake_up`。
HUD 开、HUD 关、Esc 三条关窗路径均实测进程退出。

## 3. 修复与决策记录

| # | 决策点 | 选项 | 用户选择 | 实施 |
|---|--------|------|----------|------|
| M1 | IME 预编辑串 | A 区分 NSString/NSAttributedString / B 只修 marked / C 记录 | **A** | `object_text` 按类型取 `-string`；单测 + 真实 Pinyin 回归 |
| M2 | nextDrawable / present | A presentDrawable 提前 + 超时 / B 彻底异步 / C 只超时 / D 只提前 | **A** | commit 前 `presentDrawable`；`allowsNextDrawableTimeout(true)`；nil 跳帧 |
| M2b | AX 延迟残留 | A 记录已知限制 / B 现在异步重构 / C 先完成其余 | **A** | 记录 V-114；随后 M10/M11 完成异步提交与 render 线程 |
| M3 | clippy | A 按建议修 / B allow / C 不修 | **A** | let-chain + as_chunks_mut |
| M4 | metal-real golden | A 启用 + 生成 / B 只记录 / C 生成但不比 | **A** | 删除 skip；13 份基线；文档/测试更新 |
| M5 | 全屏残留 | A 正常按钮验收 / B 修 AX 属性 / C 记录 | **A**（调查后修订） | 正常按钮路径实测通过；AX 属性记 V-115 |
| M6 | PointerLeave | A 补 tracking area / B 记录 / C 先探针 | **A** | 新增 NSTrackingArea；探针验证后去掉 `.MouseMoved` 防重复 |
| M7 | IME 焦点观察 | A 先排除搜狗 / B 直接查引擎 / C 记录 | **A** | 干净 Apple Pinyin 未复现；记 V-116 |
| M8 | DPI 变倍率 | A 切显示模式 / B 只验 2x / C 先列模式 | **A** | 1680×1050@2 ↔ 1650×1050@1 ↔ 恢复；DpiChanged 192/96。DPI scale 随 `FrameJob` 传给 render 线程（F-163） |
| M9 | 全屏/事件泵 | A common-modes 窄修复 / B 直接 NSApp.run / C 记录 | **B**（用户要求最正确修复） | NSApp.run + observer/source/timer common modes；contentView autoresizing；opaque + FullScreenPrimary |
| M10 | 渲染异步 | 最小修复 / 异步重构 / 记录 | **异步重构（macOS-only）** | commit 不 wait；3 槽轮询完成；Windows/Linux 零改动 |
| M11 | drawable/编码线程 | 专用 render 线程 / CAMetalDisplayLink / 只预取 drawable | **专用 render 线程** | F-164：`GpuRenderer` 整体移交 worker；最新帧邮箱；替换帧/无 drawable 强制全量；离屏保持同步 |
| M12 | HUD 开启退出挂起 | 撤 timer + 投 NSEvent / 只 `NSApp.stop` / 记录 | **撤 timer + 投 NSEvent** | F-165：`wake::uninstall` + `ApplicationDefined` 事件 + `CFRunLoop::stop` |

---

## 4. 修复后的验证矩阵

### 4.1 构建与门禁

| 命令 | 结果 |
|------|------|
| `cargo check -p ui_core --all-targets --features "hud,test-support"` | 通过 |
| `cargo test -p ui_core --features "hud,test-support"` | **676 + 6 + 7 + 4 + 8 + 36 = 737 passed / 0 failed** |
| `cargo test -p ui_core --release --features "hud,test-support"` | **667 + 6 + 7 + 4 + 8 + 36 = 728 passed / 0 failed** |
| `cargo +nightly clippy -p ui_core --all-targets --all-features -- -D warnings` | **通过**（0 error；只有 workspace 的 unused-dependency 提示） |
| `cargo +nightly fmt -p ui_core -- --check` | 通过 |
| `cargo doc -p ui_core --no-deps` | 通过 |
| `cargo bench -p ui_core --bench perf --no-run` | 通过 |
| shader | `xcrun metal` 真实产出 `quad.metallib`；真实窗口出帧证明加载成功（非占位符） |

### 4.2 真实窗口与渲染

| 能力 | 方法 | 结果 |
|------|------|------|
| 真实窗口出帧 | `demo --features hud`，用户身份启动 | ✅ 960×740 逻辑 → 1920×1480 物理 @2.00x Metal |
| HUD | 右上角面板 | ✅ FPS 58–60 / Frame 16.67ms / CPU 1.18–2.21ms / GPU 0.89–2.08ms / Draw 1 / Prims 251 / 1920×1480 @2.00x Metal |
| 整帧内容 | 用户身份 `screencapture -l` 窗口截图 | ✅ 六张卡片、焦点环、CJK 简/繁/日/韩、色块、PNG 图集、滚动列表 |
| resize | AX 设 1200×800（实际 1200×771）、960×740 | ✅ 物理 2400×1542 @2x；重排、无崩溃；HUD 仍 60fps |
| 全屏（HUD 关） | `AXFullScreenButton` 正常路径 | ✅ 进入 1680×1050、AXStandardWindow/子节点齐全；退出恢复 960×740，无残留（固定 960×740 布局，四周黑底属预期） |
| 全屏（HUD 开） | 连续 60fps + 全屏 | ✅ 进入/退出正常；HUD 58–60fps；完整 AX 树 0.20s；无 AXUnknown 旧快照（F-164） |
| DPI | 显示模式 1680×1050@2 ↔ 1650×1050@1 ↔ 恢复 | ✅ `DpiChanged { dpi: 192, scale: 2.0 }` / `{ dpi: 96, scale: 1.0 }`；进程存活 |
| 空闲 0 帧 | HUD 关闭、静止 2s，两次**窗口区域**截图逐字节相同 | ✅ 0.0% CPU；`sample` 主线程 `nextEventMatchingMask`，无 `frame_all` / `nextDrawable` |
| 连续帧 | HUD 开启 | ✅ 58–60fps 持续；`sample` 中 `nextDrawable` 只在 `kytrix-metal` 线程 |
| 关窗 | AXCloseButton（HUD 开/关） | ✅ 进程退出、窗口消失（F-165） |
| 最小化 | AXMinimizeButton + AXMinimized=false | ✅ 窗口消失/恢复，进程存活（无误判 close） |
| 隐藏 App | `NSRunningApplication.hide()` | ⚠️ 助手返回 false，未形成可靠结论；代码路径按 `NSApplication.isHidden` 排除误关，未实机确认 |

### 4.3 文本与 IME

| 能力 | 结果 |
|------|------|
| CJK 混排 | ✅ 简/繁/日/韩 + 英文无豆腐块，544px 内自动折行 |
| ASCII 输入 | ✅ `hello` 等直接上屏；AX value 正确 |
| 按键不丢 | ✅ 10 字符一次注入/慢速 250ms/快速 20ms 均完整（`0123456789`、`abcdefghij`） |
| 真实拼音预编辑 | ✅ 修复后 `组合中：ni'hao`、`Preedit("ni'hao")`，无属性字典 |
| 拼音上屏 | ✅ Space 选首候选 → `你好`；状态 `Commit("你好")` |
| 提交后继续输入 | ✅ 上屏「你好」后直接键入 `shi` 继续 `组合中：shi`（干净 Apple Pinyin 会话） |
| Backspace / Enter | ✅ 删除最后一个 char；Enter 提交并清空 |
| IME caret / 候选窗 | ✅ Sogou 候选窗出现在输入框 caret 附近（实测 bounds 105,785 408×60，输入框 122,754 544×54）；`firstRectForCharacterRange` 路径生效 |
| dead key | ✅ Option+E, E → `é`（ABC 键盘布局；Text 事件值正确） |
| 组合中 Esc | ✅ 窗口存活、组合取消（不关窗）；组合结束后的 Esc 走关窗路径 |
| 焦点离开取消组合 | ✅ 单测覆盖 + demo 实机 Tab 离开输入框后状态行收起 |

### 4.4 交互

| 能力 | 结果 |
|------|------|
| Tab 焦点环 | ✅ 1→2→3→选项A→选项B→滑块→输入框→回绕；焦点面板同步 |
| Shift+Tab | ✅ 从色块 1 回绕到输入框 |
| Enter 激活 | ✅ 焦点色块 1 时 Enter → `选中：色块 1（红） · #E5484D`，剪贴板同步 |
| 指针点击 | ✅ 色块 3 → `选中：色块 3（蓝） · #0091FF`，焦点 3/7，剪贴板 `#0091FF` |
| 方向键 | ✅ 滑块 60→100→90；AX value 与状态行同步（`方向键 → 90%`） |
| 滚轮（非精确） | ✅ 一格 `-3` → 48px（2 行）；20 行滚到底夹紧到 354px；滚回顶部 |
| 滚轮（精确） | ✅ 20px → 2 行（96px），符合 `precise / 10` 的既有实现 |
| PointerMove / PointerLeave | ✅ 临时 event probe：进入窗口收到 `PointerMove`；移出收到 `PointerLeave`；每个移动只 1 条（修复前 2 条） |
| F12 HUD | ✅ 隐藏/显示；全帧重绘清除残留 |
| 光标形状 | ⚠️ v1 只支持系统箭头（V-92 既有边界） |

### 4.5 无障碍（AX API）

| 能力 | 结果 |
|------|------|
| AX 树 | ✅ frame + 标题 + 色块/单选/滑块/输入框/图片；角色/名称/描述/值/checked 齐全（36–48 行树，随 depth） |
| AX 动作回投 | ✅ AXPress 色块 2 → `选中：色块 2（绿） · #30A46C`；剪贴板 `#30A46C` |
| 单选 | ✅ AXPress 选项 B → checked 翻转，状态 `Click → 选项 B` |
| Slider SetValue | ✅ 设 AXValue=80 → `缩放 80%`、状态 `SetValue → 80%` |
| TextInput SetValue | ✅ 设 AXValue=`无障碍输入` → value/状态行更新 |
| AX 焦点 | ✅ 对滑块设 `AXFocused=true` → 焦点面板 `缩放滑块`、Tab 序 6/7 |
| AX 命中测试 | ✅ `AXUIElementCopyElementAtPosition` 在色块/输入框/标题处返回正确节点 |
| AX 延迟 | ✅ HUD 关闭完整树约 0.12s；HUD 开启全屏 60fps 下 74 行树 **0.20s**（F-164；V-114/V-119 关闭） |
| VoiceOver / Orca | ⚠️ 未接真实读屏器；只验 AX API 层 |

### 4.6 剪贴板

| 方向 | 命令 | 结果 |
|------|------|------|
| 引擎 → 外部 | 点击色块后外部 `pbpaste` | ✅ `#E5484D` / `#0091FF` / `#30A46C` |
| 外部 → 引擎 | `pbcopy` 写入 `from-outside-你好`，`App::clipboard_text()` | ✅ 原样读回 |
| 空剪贴板 | `pbcopy` 空输入，`clipboard_text()` | ✅ 返回空串 |

临时验证程序使用 public `App::set_clipboard_text` / `App::clipboard_text`，并直接调用系统
`pbpaste` / `pbcopy`，验证后已删除。

### 4.7 离屏快照 / golden

| 能力 | 结果 |
|------|------|
| `snapshot_demo --snapshot` | ✅ 960×740 PNG；IME 种子态（preedit `shi li`）、CJK、PNG 图集全部正确 |
| macOS 基线 | ✅ `tests/golden/metal-real/` 13 份；生成后同帧读回自检 `max=0 avg=0.000 over=0` |
| 正常比对 | ✅ `cargo test ... --test snapshot` 8 passed / 0 failed |
| 跨设备差异 | ✅ metal-real vs warp：13/13 case `max=1`、`over=0`；metal-real vs lavapipe：`max` 64–164、`mean` 0.0048–0.0774、`over` 3–105，与 warp vs lavapipe 同级（§5） |

### 4.8 截图弹窗（环境，不是引擎 BUG）

root 身份调用 `screencapture` 会触发 macOS 15 的
「正在请求绕过系统隐私窗口选择器，直接访问您的屏幕和音频」确认；同一命令以 console 用户运行
（`sudo -u chenbaiyu screencapture …`）不再弹窗，窗口/全屏截图均成功。已在
`ScreenCaptureApprovals.plist` 里把 `com.googlecode.iterm2` 的审批日期设到未来（2126），
但真正避免弹窗的是**用用户身份截图**。后续验证全部改用用户身份截图；不再需要人工点授权。

---

## 5. metal-real vs warp / lavapipe 的量化差异

同一 case 在三份 golden 上的逐通道差（`Tolerance::default()`，`per_channel=1`）：

| case | metal-warp max / over | metal-lavapipe max / mean / over |
|------|----------------------|----------------------------------|
| `clip_1_light` | 1 / 0 | 135 / 0.0115 / 17 |
| `colors_1_dark` | 1 / 0 | 67 / 0.0471 / 29 |
| `colors_1_light` | 1 / 0 | 131 / 0.0774 / 23 |
| `gradient_1_light` | 1 / 0 | 135 / 0.0486 / 26 |
| `image_1_light` | 1 / 0 | 64 / 0.0088 / 3 |
| `image_2_light` | 1 / 0 | 64 / 0.0048 / 3 |
| `shadow_1_light` | 1 / 0 | 64 / 0.0219 / 105 |
| `shapes_1.25_dark` | 1 / 0 | 99 / 0.0113 / 56 |
| `shapes_1.25_light` | 1 / 0 | 156 / 0.0080 / 58 |
| `shapes_1_dark` | 1 / 0 | 102 / 0.0151 / 62 |
| `shapes_1_light` | 1 / 0 | 164 / 0.0191 / 63 |
| `shapes_2_dark` | 1 / 0 | 102 / 0.0040 / 52 |
| `shapes_2_light` | 1 / 0 | 131 / 0.0049 / 81 |

**读法**：metal-real 与 WARP 只差 1 个通道级，13/13 case 的超容差像素为 0；与 lavapipe 的差异
与 Linux 文档里 warp vs lavapipe 几乎同分布，集中在 AA 边缘与阴影渐变。按 D14 仍各用各的目录：
驱动/版本变化可能让真实 GPU 的 AA 舍入漂移，不把跨设备容差当作冻结契约。

---

## 6. 未覆盖 / 遗留

| 项 | 说明 |
|----|------|
| 多显示器 / 混合 DPI | 只验单内建 Retina + 1x/2x 显示模式切换；无外接显示器 |
| 真实 VoiceOver 联动 | 只验 AX API；未开 VoiceOver 做朗读/焦点环联动 |
| Sogou 候选窗焦点 | 早期按键不进入应用的现象未在干净 Apple Pinyin 复现；第三方 IME 候选窗行为未定位。V-116 |
| App 隐藏 | `NSRunningApplication.hide()` 助手返回 false；未可靠实测（代码按 `isHidden` 排除误关） |
| 窗口拖拽/双击标题栏 | macOS 自绘标题栏没有原生拖拽区（F-127 的 P13 范围）；未验 |
| 最小化/缩放按钮 | 按 F-127 隐藏；只通过 AX 属性按钮做生命周期测试 |
| 设备丢失 / GPU 重置 | 未构造 Metal 设备移除；只验正常路径 |
| GPU capture | 未接 `MTLCaptureManager`（V-90 既有边界） |
| 热插拔/多窗口 | demo 单窗口；多窗口广播未在本轮重测 |
| 屏幕捕获弹窗 | 环境问题；用 console 用户身份截图绕开（§4.8） |

### 6.1 验证夹具的坑

1. **root 截图会弹授权框**：agent 以 root 运行，`screencapture` 触发 macOS 15 的 SCK 绕过选择器确认；
   改用 `sudo -u chenbaiyu` 后无弹窗。不要用 root 截图做自动化。
2. **Esc 会关 demo**：demo 的 Esc 在非组合态就是退出；测试 IME 取消组合前必须先确认焦点在输入框且
   确实处于组合态，否则会把「应用退出」误判成崩溃。
3. **窗口位置会变**：AX/screencapture 使用屏幕点坐标；demo 默认 80,230 960×740，但全屏/DPI 切换后
   必须重新读 `CGWindowList` 的 bounds，不能硬编码。
4. **Sogou 候选窗是独立进程/窗口**：搜索或截全屏时它会出现在窗口列表；它不是 demo 的窗口，不能计入
   demo 的重复窗口判断。
5. **AX 树遍历曾会被连续帧拖慢**：F-164 后 HUD 60fps 下完整树 0.20s，不再需要先关 HUD；若再遇超时，
   先确认 worker 是否停在 `nextDrawable`（`sample` 的 `kytrix-metal` 线程），而不是 AX 死锁。

---

## 7. 复现命令速查

```bash
# 门禁
cargo test -p ui_core --features "hud,test-support"
cargo test -p ui_core --release --features "hud,test-support"
cargo +nightly clippy -p ui_core --all-targets --all-features -- -D warnings
cargo +nightly fmt -p ui_core -- --check
cargo doc -p ui_core --no-deps

# 真实窗口（需要 Aqua 会话；以 console 用户启动，避免 TCC 归因问题）
sudo -u chenbaiyu cargo run -p ui_core --example demo --features hud

# 用户身份截窗口（不触发 macOS 15 录屏确认）
sudo -u chenbaiyu screencapture -x -o -l <window-id> out.png

# 离屏快照
cargo run -p ui_core --example snapshot_demo --features test-support -- --snapshot out/demo.png

# 像素 golden（macOS 生成 / 比对）
UPDATE_GOLDEN=1 cargo test -p ui_core --features test-support --test snapshot -- --nocapture
cargo test -p ui_core --features test-support --test snapshot

# 显示模式 2x↔1x（示例 helper，仓库外）
swiftc displaymode.swift -o displaymode
./displaymode list
./displaymode set1x 1650 1050   # 切到 1x，3s 后恢复原模式
```
