# Windows 平台集成审查

审查对象：`crates/ui_core/src/platform/windows/`（event_loop.rs、window.rs、composition.rs、dpi.rs、ime.rs、clipboard.rs、a11y.rs）、`crates/ui_core/src/platform/{dispatcher,event,ime}.rs` 与 `platform/windows.rs`。基线：HEAD `a9a4220`，只读审查。

## 总体判断

整体工程质量高于一般自研平台层：作者对 Win32 消息语义、DirectComposition 的交换链约束、DPI 与 IME 的 probe 结果都写进了注释和测试，消息泵采用“WndProc 只入队、泵统一消费”的结构，正常关闭路径也刻意保证了 GPU 线程先退出、HWND 后销毁。

但对照 Win32 官方语义与 Chrome/Qt 的 Windows 平台层，仍有若干会在真实桌面环境暴露的问题：剪贴板读取存在无边界内存扫描（唯一的 P1）；非客户区最小尺寸、`WM_DPICHANGED`、`WM_SETCURSOR`、多窗口指针状态、IME 启用状态与关闭语义集中存在 P2 缺口。若 Kytrix 要替代 GPUI 并正式支持多窗口、中文/日文 IME 与交易场景的键盘快捷键，建议先修复 P1/P2；如果短期只支持单窗口，也应在平台上显式限制并记录，而不是保留当前“首窗口服务”的隐式行为。

## 发现（P0-P3）

### P1-1 剪贴板读取不对 HGLOBAL 做长度校验，存在越界读

- 证据：`crates/ui_core/src/platform/windows/clipboard.rs:105-117`。`get_text` 取得 `GlobalLock` 指针后按 `u16` 逐字扫描到第一个 NUL，完全没有调用 `GlobalSize`，也没有最大长度限制。
- 问题：`CF_UNICODETEXT` 的数据来自其它进程，Windows 只保证“约定上以 NUL 结尾”，不保证对恶意/异常写入者成立。剪贴板内容可被任意进程设置；若全局内存块在分配长度内没有 NUL，`pointer.cast::<u16>().add(index)` 会越过 `GlobalAlloc` 的块尾继续读，属于未定义行为/访问违例。
- 影响：用户从外部程序复制一段异常文本后，Kytrix 执行粘贴或任何 `clipboard_text()` 读取即可能崩溃，且属于可被第三方程序触发的稳定性/安全问题。`set_text` 在 `SetClipboardData` 失败时也没有 `GlobalFree` 已分配块（`clipboard.rs:56-75`），是同一 API 族的资源泄漏。
- 成熟实现对照：Win32 剪贴板读取必须先用 `GlobalSize` 限定 `HGLOBAL` 的可读字节数；Chrome/Qt 的 Windows 剪贴板实现都以格式数据长度或 `GlobalSize` 为边界，并把“没有 NUL”当作无效数据而不是继续扫描。
- 建议：用 `GlobalSize(global)` 得到字节上限，按 `size / 2` 限制扫描长度；遇到奇数长度、空块、无 NUL 时返回 `EngineError::Clipboard` 或空串（两者择一并在 API 文档写明），绝不越过块尾。`set_text` 的失败分支补 `GlobalFree(global)`。
- 置信度：高。代码路径确定；用手工构造的无 NUL `CF_UNICODETEXT` 即可复现（不需要外部恶意程序，测试程序即可写入）。

### P2-1 `WM_GETMINMAXINFO` 把“最小客户区”当成“最小窗口尺寸”

- 证据：`crates/ui_core/src/platform/windows/event_loop.rs:349-358` 将 `state.min_size` 直接写入 `MINMAXINFO.ptMinTrackSize`；`crates/ui_core/src/platform/windows/window.rs:231-238` 只在逻辑最小值上乘 DPI，没有加系统标题栏/边框；自定义缩放的 `resized_rect` 也在 `event_loop.rs:141-156` 用同一个值直接钳制**窗口外框**。
- 问题：`WM_GETMINMAXINFO` 的 `ptMinTrackSize` 语义是“用边框能产生的最小**窗口**尺寸”（包括非客户区），而不是客户区尺寸。当前实现把 320×200（逻辑）的客户区目标当作窗口外框目标；在 96 DPI 下实际客户区会少掉标题栏与边框（约少 16 px 宽、39 px 高，随主题/DPI 变化），窗口可以被缩到低于布局所需的客户区。
- 影响：图表/工具栏的最小布局会被压坏，出现裁剪、文本溢出或布局重排；`WM_GETMINMAXINFO` 和自绘边框缩放两条路径都不一致地保护最小客户区，属于用户日常拖动窗口即可触发的 P2。
- 成熟实现对照：Chromium `HWNDMessageHandler::OnGetMinMaxInfo` 在 `WidgetSizeIsClientSize()` 时调用 `InflateClientSizeConstraintsInPixels(hwnd(), min_window_size, max_window_size)`，明确把客户端最小尺寸膨胀为窗口尺寸；Qt 同样用 `AdjustWindowRectExForDpi` 换算。
- 建议：统一从“最小客户区（逻辑）→ `AdjustWindowRectExForDpi` → 最小窗口外框（物理）”计算 `ptMinTrackSize`，并让 `resized_rect` 使用同一外框最小值；同时考虑 `WS_EX_NOREDIRECTIONBITMAP` 的实际 exStyle 传入 `AdjustWindowRectExForDpi`。
- 置信度：高。

### P2-2 `WM_DPICHANGED` 先按 `(100, 100)` 重排窗口，再套系统建议矩形，造成跳变和重复布局

- 证据：`crates/ui_core/src/platform/windows/event_loop.rs:887-901` 的 `Dpi` 分支先调用 `native.apply_scale(scale, dpi)`，随后再用系统 `suggested` 调 `native.place(rect)`；`crates/ui_core/src/platform/windows/window.rs:343-378` 的 `apply_scale` 内部执行 `SetWindowPos(hwnd, None, 100, 100, window_size...)`。
- 问题：`WM_DPICHANGED` 的标准处理是把窗口直接设为 `lParam` 给出的建议矩形（该矩形已按新 DPI 保持逻辑大小并考虑了工作区）。当前代码先按固定 `(100, 100)` 把窗口移到主屏/左上角一次，再移回系统建议位置；每次 `SetWindowPos` 都产生 `WM_SIZE`/布局失效，连续两次还会破坏最大化/跨屏拖动的视觉状态。如果 `place` 因任何原因失败，窗口会停留在 `(100, 100)`。
- 影响：跨显示器拖动、显示缩放切换时窗口闪烁并跳到错误位置；高频图表窗口会触发两轮完整布局/重绘，过渡帧可见；在最大化跨屏场景可能丢失最大化状态。
- 成熟实现对照：Chromium `HWNDMessageHandler::OnDpiChanged` 只调用 `SetBoundsInternal(gfx::Rect(*reinterpret_cast<RECT*>(l_param)), false)`，不做额外的一次 `(100,100)` 重排；Qt 的 `QWindowsWindow::handleDpiChange` 同样直接使用建议矩形。
- 建议：`WM_DPICHANGED` 只应用 `suggested` 矩形；只有在 `lParam == 0` 时才用当前逻辑尺寸按新 DPI 重算窗口尺寸。先更新 scale/min_size，再 `SetWindowPos`，并保留最大化状态（窗口最大化时不要改写 restore 位置）。
- 置信度：高。

### P2-3 没有处理 `WM_SETCURSOR`，应用自定义光标会被 `DefWindowProcW` 重置为箭头

- 证据：`crates/ui_core/src/platform/windows/event_loop.rs:74-92` 定义 `cursor_handle`；`event_loop.rs:722-727` 在泵里用 `SetCursor` 应用应用层的光标变化；但 `WndProc` 没有 `WM_SETCURSOR` 分支，`translate` 也不处理该消息，最终落到 `event_loop.rs:531` 的 `DefWindowProcW`。窗口类光标固定为箭头（`window.rs:437-439`）。
- 问题：`WM_SETCURSOR` 会在鼠标移动时发送给窗口，`DefWindowProc` 会用注册类光标（此处为 `IDC_ARROW`）设置光标。应用在泵里 `SetCursor(IDC_HAND/IDC_IBEAM/...)` 后，下一次鼠标移动的 `WM_SETCURSOR` 就把光标改回箭头；由于 `take_cursor_change` 是一次性的，悬停节点不变时不会再次设置，最终用户看不到手型/文本光标。
- 影响：按钮、链接、文本输入框、可拖拽区域的 hover 光标失效，用户对可交互性的感知下降；对交易软件的图表拖拽/十字光标尤其明显。这是 P2 级别的可直接观察 UX 回归。
- 成熟实现对照：Win32 文档明确 `DefWindowProc` 对 `WM_SETCURSOR`“sets the cursor to an arrow if it is not in the client area, or to the registered class cursor if it is in the client area”；Qt 在 `QWindowsWindow::handleSetCursor`/`WM_SETCURSOR` 中设置并返回 `TRUE`，Chromium 也在消息映射中接管 `WM_SETCURSOR`。
- 建议：在 `WndProcState` 保存“期望光标”，`WndProc` 处理 `WM_SETCURSOR`：当 `LOWORD(lParam)` 在客户区且命中当前 desired cursor 时调用 `SetCursor` 并返回 `TRUE`；否则交回 `DefWindowProcW`。泵里的 `cursor_handle` 计算可以保留，但最终设置应经由 `WM_SETCURSOR`。
- 置信度：高。

### P2-4 多窗口路径把指针捕获、光标和连续帧定时器硬编码到 `windows.first_mut()`，且首窗关闭后定时器不迁移

- 证据：`crates/ui_core/src/platform/windows/event_loop.rs:722-751`：光标变化、`app.take_capture_request()`、`SetTimer(FRAME_TIMER_ID)` 全部只对 `windows.first_mut()` 执行；`event_loop.rs:947-955` 的 `retarget_wake` 只改 `dispatcher.attach` 与剪贴板 owner，没有迁移 `frame_timer`。`app.rs:2131-2137` 的 `take_cursor_change/take_capture_request` 是 App 级单槽，不带 `WindowId`。
- 问题：如果第二个窗口内的可拖拽控件请求捕获，平台层会把 `SetCapture` 设到第一个窗口；之后所有指针消息都进入第一个窗口的 `WndProc`，再按第一个窗口 id 分发给 App，导致第二窗口拖拽事件丢失/串窗。`frame_timer` 绑定首窗 HWND，首窗关闭后即使还有窗口存活，`Some(id)` 仍使 `needs_continuous_frames() == true` 的分支不再 `SetTimer`，剩余窗口的动画/HUD 永久停帧。
- 影响：多窗口是 `App::windows()` 与 `run()` 明确支持的模型；当前实现下第二窗口的拖拽、hover 光标、动画都有错误。若产品实际只做单窗，应在 `run`/`App` 层显式拒绝多窗；否则这是 P2。
- 成熟实现对照：Chrome/Qt 的鼠标捕获、光标与动画时钟都是 per-native-window 状态；`SetCapture` 必须由收到按下消息的那个 HWND 调用，窗口销毁时对应定时器/帧源随窗口对象一起迁移或销毁。
- 建议：把 `cursor_pending`、`capture_request`、`frame_timer` 改为 `(WindowId, ...)` 并放在每个 `NativeWindow` 上；`WM_CAPTURECHANGED`/窗口销毁时只清理该窗口状态；`retarget_wake` 同时把 frame timer 从被销毁窗口迁到存活窗口。
- 置信度：高（代码事实明确；影响取决于是否正式启用多窗口，但 `run` 已按多窗口构造）。

### P2-5 应用层捕获丢失（`WM_CAPTURECHANGED`）只在自绘边框缩放分支处理，普通拖拽会卡死

- 证据：`crates/ui_core/src/platform/windows/event_loop.rs:407-413` 的 `WM_CAPTURECHANGED` 只在 `state.resize.is_some()` 时收尾；普通 App 捕获（`event_loop.rs:728-735`）没有对应的丢失处理。`app.rs:936-943` 的 `on_pointer_down` 在 `pointer.drag.is_some()` 时直接返回；`app.rs:1276-1293` 只在 `FocusChanged(false)` 时取消拖拽。
- 问题：`WM_CAPTURECHANGED` 是 Win32 通知窗口“鼠标捕获已被系统/其它窗口拿走”的唯一可靠消息；它可以由 Alt+Tab 以外的原因触发（另一个窗口调用 `SetCapture`、系统菜单、触控笔/弹出窗口等），此时窗口不一定收到 `WM_KILLFOCUS`。App 的 `DragSession` 仍保持 `Some`，`on_pointer_down` 的早退会吞掉后续所有按下，直到某次失焦才恢复，表现为“鼠标点不动”。
- 影响：拖拽进行中切到系统菜单/其它窗口后，Kytrix 可能永久处于拖拽状态；之后的点击、图表交互和按钮都失效。P2，且用户可通过常规窗口操作触发。
- 成熟实现对照：Chromium `HWNDMessageHandler::OnCaptureChanged(HWND)` 直接调用 `delegate_->HandleCaptureLost()`；Qt 的 `WM_CAPTURECHANGED` 也清理鼠标抓取状态。`WM_CANCELMODE` 同样需要清理。
- 建议：在 `WndProc` 里把 `WM_CAPTURECHANGED` 从“仅 resize 分支”提升为通用分支：若 `state.resize` 存在则走现有收尾，否则向 App 投递 `PointerCancel`/`Drag Cancel` 并清空 `pointer.drag`/`press`；同时处理 `WM_CANCELMODE`。
- 置信度：高。

### P2-6 键盘修饰键按消息顺序维护，失焦时不复位，Alt+Tab 后 Shift/Ctrl 会“粘住”

- 证据：`crates/ui_core/src/platform/event.rs:126-135` 只在 `WM_KEYDOWN/WM_KEYUP` 中调用 `update_modifiers`；`event.rs:213-222` 没有系统状态校验；`event_loop.rs:450-456` 的 `WM_KILLFOCUS` 只同步 a11y 和 `PlatformEvent::Focus(false)`，不清 `TranslateState.modifiers`。App 的 `on_window_focus_lost`（`app.rs:1276-1299`）也只清拖拽/悬停，不涉及平台修饰键。
- 问题：按住 Shift/Ctrl/Alt 时 Alt+Tab 到其它窗口、锁屏或使用 Win 键会丢失 `WM_KEYUP`；回来之后 `TranslateState.modifiers` 仍保留按下位，后续所有鼠标/键盘事件都带错误修饰键。对使用 `Shift+滚轮`、`Ctrl+点击`、快捷键的交易界面，这会造成错误操作或滚动轴错误。
- 影响：P2 输入正确性问题，且普通用户很容易触发；在 K 线/画线工具中可能导致误拖拽、误选择。
- 成熟实现对照：Chromium `ui::GetModifiersFromKeyState()` 在每个输入事件用 `GetKeyState` 读取实时修饰键，而不是依赖消息顺序；Qt 的 `QWindowsKeyMapper` 也会在 focus 变化/`WM_ACTIVATE` 时同步键盘状态。
- 建议：`WM_KILLFOCUS`/`WM_ACTIVATEAPP(FALSE)` 时清空 `TranslateState.modifiers`（并可向 App 发合成 KeyUp，避免 App 内部按键状态残留）；更稳妥的是在 `translate` 入口用 `GetKeyState(VK_SHIFT/CONTROL/MENU/LWIN/RWIN)` 生成 `Modifiers`，消息顺序值仅作补充。
- 置信度：高。

### P2-7 IME 始终开启，没有 `ImmAssociateContext(NULL)` 的非文本输入禁用路径

- 证据：`crates/ui_core/src/platform/windows/ime.rs:31-93` 的 `ImeState` 只有 caret 区域/组合串状态，没有任何 `ImmAssociateContext` 或 `ImmAssociateContextEx` 调用；`window.rs:316-335` 在 `ime_cursor_area == None` 时只清 hint，不关闭 IME。`WndProc` 对所有窗口一律把 `WM_IME_*` 交给 `ImeState`。
- 问题：当焦点不在文本输入框（图表、表格、工具栏）时，如果用户启用了中文/日文 IME，键盘输入仍进入 IME 组合，普通字符会变成 `WM_IME_*`/`VK_PROCESSKEY`，应用收不到预期的字母/数字快捷键；候选窗还会覆盖图表。Qt 在焦点对象不支持输入时显式调用 `ImmAssociateContext(hwnd, nullptr)` 禁用 IME，只对可输入控件恢复默认 context。
- 影响：交易软件大量使用键盘快捷键（买卖、切换周期、搜索）；IME 未关闭会导致快捷键失效或误触发中文候选，属于中文 Windows 环境的 P2 体验问题。
- 成熟实现对照：Qt `QWindowsInputContext::setWindowsImeEnabled()`：`ImmAssociateContextEx(hwnd, nullptr, IACE_DEFAULT)` / `ImmAssociateContext(hwnd, nullptr)`；Chrome 也根据 `TextInputClient` 的 `HasComposition`/焦点对象切换 IME 输入范围。
- 建议：由 App 暴露“当前窗口是否有可输入焦点/是否接受 IME”的状态（可从 `ime_cursor_area` 的有无推导，但最好显式）；平台层在状态变化时调用 `ImmAssociateContext(hwnd, NULL)` 或 `ImmAssociateContextEx(..., IACE_DEFAULT)`，并清理组合状态。
- 置信度：高（问题机制明确；当前 demo 的输入框场景可能因 IME 默认关闭而未暴露）。

### P2-8 `WM_CLOSE` 无条件关窗，`CloseRequested` 只是观察事件，无法拦截；`WM_QUERYENDSESSION` 未处理

- 证据：`event_loop.rs:518-529` 对 `WM_CLOSE` 只设置 `state.close_requested = true` 并返回 0，不调用 `DefWindowProc`；`event_loop.rs:923-942` 下一轮无条件 `app.close_window(id)` + `native.destroy()`。`PlatformEvent::Close` 在 `event_loop.rs:916` 映射为 `Event::CloseRequested`，但 App 没有 veto API；`event.rs` 的 `translate` 只处理到 `WM_CLOSE`（`event.rs:171`），没有 `WM_QUERYENDSESSION/WM_ENDSESSION`。
- 问题：用户点标题栏 X、任务栏关闭或 Alt+F4 时，窗口一定被销毁；应用无法弹出“未保存的订单/布局，确认退出吗？”或取消关闭。OS 注销/关机时 `WM_QUERYENDSESSION` 由 `DefWindowProc` 默认返回 TRUE，Kytrix 没有机会保存状态。
- 影响：交易应用的未提交表单、布局状态可能丢失；对需要二次确认的操作（平仓确认、下单确认）尤其危险。属于 P2 生命周期/产品安全问题。
- 成熟实现对照：Qt 的 `QCloseEvent::ignore()` 可阻止关闭；Chromium `HWNDMessageHandler::OnClose` 交给 `delegate_->HandleClose()`，由上层决定是否 `DestroyWindow`；`WM_QUERYENDSESSION` 是 Windows 应用保存状态的官方入口。
- 建议：把 `WM_CLOSE` 转为 `Event::CloseRequested` 后由 App 决定 `close_window`/忽略；平台层增加 `Window::request_close` 或 `App::confirm_close` 回调；至少为 `WM_QUERYENDSESSION` 提供可注册的保存钩子并返回 `TRUE/FALSE`。
- 置信度：高。

### P2-9 标题栏拖动/系统菜单仍会进入 `DefWindowProc` 模态循环，泵阻塞且 `WndProcState.events` 积压

- 证据：`event_loop.rs:377-386` 的注释只说明已接管“自绘边框缩放”的 `WM_NCLBUTTONDOWN`，但 `is_resize_hit` 不包含 `HTCAPTION`（`event_loop.rs:95-100`）；标题栏按下最终落到 `event_loop.rs:531` 的 `DefWindowProcW`，进入 Win32 内置移动/菜单模态循环。`WndProcState.events` 只在泵的 `collect_events`（`event_loop.rs:799-812`）消费，模态循环期间所有 `WM_MOUSEMOVE/WM_TIMER/WM_PAINT` 事件只会 push 到 `events`（`event_loop.rs:223-229`）。
- 问题：拖动标题栏、打开系统菜单、使用 Alt+Space 时，`DispatchMessageW` 被 `DefWindowProc` 的模态循环占住，泵无法执行 `app.run_frame_hook`/`frame_all`/`collect_events`；期间鼠标移动事件大量积压，松手后一次性回放为过时的 `PointerMove` 序列，可能触发错误的 hover/拖拽状态。动画和 HUD 在拖动期间完全停帧。
- 影响：长距离拖动窗口后出现卡顿/指针轨迹错乱；多显示器交易员频繁挪窗时体验明显下降。若将来接入 `TrackPopupMenu` 等菜单，同一问题会扩大。
- 成熟实现对照：Chromium 用 `MoveLoopMouseWatcher` 监视 `WM_LBUTTONUP/Escape` 并实现自定义移动循环，`OnEnterSizeMove/OnExitSizeMove` 由 delegate 处理；Qt 的 Windows 平台层也在 `WM_ENTERSIZEMOVE` 期间维持事件处理，而不是把整个泵阻塞在内核模态循环里。
- 建议：对 `HTCAPTION` 也接管 `WM_NCLBUTTONDOWN` 并实现与边框缩放一致的定时器合并移动循环；或在 `WndProc` 中只记录最新指针位置（像 `ResizeSession.latest` 那样）而不是每个 `WM_MOUSEMOVE` 都 push。至少要在 `WM_ENTERSIZEMOVE/WM_EXITSIZEMOVE` 处做事件队列上限/合并，并在退出时重新同步光标。
- 置信度：高。

### P3-1 IME 提交去重按 Unicode 标量计数，而非 `WM_CHAR` 的 UTF-16 码元

- 证据：`crates/ui_core/src/platform/windows/ime.rs:41-45` 在 `GCS_RESULTSTR` 后用 `text.chars().count()` 设置 `suppress_chars`；`crates/ui_core/src/platform/event.rs:136-142` 每收到一条 `WM_CHAR` 就 `take_char_suppression()` 减一。Win32 文档明确 `WM_CHAR` 的 `wParam` 是 UTF-16 码元，且 Vista 起可把代理对拆成两条 `WM_CHAR`（[WM_CHAR 文档](https://learn.microsoft.com/en-us/windows/win32/inputdev/wm-char)）。
- 问题：若 IME 提交结果包含“非 BMP 字符 + 后续 BMP 字符”（例如 `🌍a`），`chars().count()` 为 2，但 `DefWindowProc` 产生的 `WM_CHAR` 是 3 个码元；前两个被吞掉后，尾随 `a` 会再插入一次，造成文本重复。反过来若 IME 只发 `WM_IME_CHAR` 而不发 `WM_CHAR`，`suppress_chars` 会残留并吞掉下一次真实键盘输入。`is_ime_message` 也没有覆盖 `WM_IME_CHAR`（`event_loop.rs:535-539`）。
- 影响：中文/日文 IME 输入包含 emoji 或补充平面字符时可能重复/丢字；属于 P3 的边界输入 bug，但会随着 emoji 使用增多而更容易遇到。
- 成熟实现对照：Qt 的 `QWindowsInputContext::composition` 直接消费 `GCS_RESULTSTR` 并向焦点对象发 `QInputMethodEvent`，不依赖 `DefWindowProc` 产生的 `WM_CHAR`；对 `WM_IME_CHAR` 有独立处理。Chrome 的 IME 输入路径同样以 `GCS_RESULTSTR` 为准，避免重复提交。
- 建议：把 `suppress_chars` 改为 `text.encode_utf16().count()`，并在 `is_ime_message` 中处理 `WM_IME_CHAR`（与 `WM_CHAR` 一样做文本/抑制）；更彻底的做法是 `WM_IME_COMPOSITION` 已消费 `GCS_RESULTSTR` 时不再把消息交给 `DefWindowProcW`，从源头避免重复 `WM_CHAR`。
- 置信度：中高。`WM_CHAR` 的码元语义确定；具体某款 IME 是否会在 `GCS_RESULTSTR` 后再发 `WM_CHAR`/`WM_IME_CHAR` 需在目标输入法矩阵实测。

### P3-2 启动失败与 panic 展开路径的销毁顺序与正常路径相反，可能先销毁 HWND 再等 GPU 线程退出

- 证据：`event_loop.rs:598-641` 的 `run(mut app: App)` 在局部 `windows`（`event_loop.rs:610`）之后仍持有 `app`；任一 `NativeWindow::create`/`Renderer::new_for_hwnd` 的 `?` 提前返回时，Rust 按逆序析构局部变量，`windows` 先于参数 `app` 析构。`pump`（`event_loop.rs:643-655`）的 `drop(app); drop(windows);` 只覆盖正常返回；若 `pump_loop` panic 展开，函数参数按 `dispatcher, windows, app` 的逆序析构，同样会先销毁原生窗口。
- 问题：第一个窗口已挂上 GPU 线程后，第二个窗口创建/渲染器初始化失败（或 `pump` 内任何 panic）会在 App 仍持有 `Renderer`、GPU 线程仍可能 `Present` 时先执行 `NativeWindow::destroy`，违反代码自己声明的“先 join GPU 线程再销毁 HWND”不变量。
- 影响：启动失败或 panic 时可能出现 GPU 线程访问已销毁 HWND 的错误/崩溃，掩盖真正的失败原因；P3（非正常路径），但属于生命周期安全应修项。
- 成熟实现对照：Chrome/Qt 用 RAII 明确保证 window 对象比渲染/Gpu 线程活得久，关闭顺序由 owner 的析构顺序和显式 shutdown 统一；不会只保护正常返回路径。
- 建议：在 `run` 中用 guard（先声明 `App` 的 drop guard，保证最后析构）或把 `NativeWindow` 集合放进 `App` 关联的生命周期对象；`pump` 也使用 guard 使 panic 路径保持 `app` 先于 `windows` 释放，或在 `catch_unwind` 中显式执行 `drop(app); drop(windows);` 后重新抛出。
- 置信度：中高（Rust 析构顺序确定；实际崩溃取决于 GPU 线程与 HWND 的竞态）。

## 做得好的地方

- `WndProc` 严格只做状态收集、不调用 `App`，并把 `WM_QUIT` 的“`PeekMessageW` 会消费掉”这个 probe 结论写进泵的显式判断（`event_loop.rs:666-675`），避免了常见的永久阻塞。
- 唤醒合并（`frame_pending` 电平信号）与 `PostThreadMessageW` 兜底设计完整，`Dispatcher` 测试覆盖了“窗口目标失效后仍能唤醒”的 P14 回归场景（`dispatcher.rs:258-296`）。
- DirectComposition 采用内容 visual 恒 1:1 + 1×1 背景层放大的 Chrome 式设计，避免了 resize 时旧帧被拉伸/回弹；buffer 采用 grow-only 高水位 + 256 对齐桶，并正确地在 `ShowWindow` 前 prime 内容和背景（`composition.rs:1-37`、`112-176`）。
- 自绘边框缩放用定时器合并到约 60 Hz，并且把最终 `SetWindowPos` 收口到 `apply_resize`，规避了每个 `WM_MOUSEMOVE` 都重排窗口造成的 P1-2b 性能回退（`event_loop.rs:250-329`、`1172-1206`）。
- IME caret 区域换算成物理像素，同时设置 `COMPOSITIONFORM` 与 `CANDIDATEFORM`，并缓存 `(Rect, Scale)` 在 DPI 变化后重发（`ime.rs:96-128`、`window.rs:315-335`）；单测覆盖了表单映射和缩放。
- a11y 采用延迟激活、`in_getobject` 重入旗标、事件 raise 前释放 `&mut WndProcState` 的借用纪律，这些都是 AccessKit/Windows UIA 集成的正确姿势（`a11y.rs:28-109`、`event_loop.rs:419-456`）。
- 正常关闭路径明确先 drop `App`（join GPU 线程）再销毁 `NativeWindow`，并在 `NativeWindow::destroy` 中先 `composition.shutdown()` 再 `DestroyWindow`（`event_loop.rs:643-655`、`window.rs:390-405`），方向正确。
- 测试密度高：`resize_tests` 覆盖八方向/最小尺寸，`wndproc_tests` 覆盖合成唤醒与缩放节流，`ime.rs`/`a11y.rs` 有隐藏窗口 smoke test，说明作者对回归敏感。

## 存疑 / 需实测

- **最小化时的帧循环**：`event_loop.rs:696-719` 在几何 pass 中跳过 `IsIconic`，但 `frame_all`（`event_loop.rs:958-967`）与 `FRAME_TIMER_ID` 逻辑（`event_loop.rs:737-751`）没有按最小化暂停；`WM_SIZE` 的 `SIZE_MINIMIZED` 也在 `event.rs:147-155` 被丢弃。若窗口有持续动画/HUD，最小化后仍可能 60 Hz 绘制/提交。建议实测最小化 10 分钟的 GPU/CPU 占用；若高，应在 `WM_SIZE(SIZE_MINIMIZED)` 停 timer、恢复时重启。
- **触摸/触控板/笔**：当前 `event.rs` 只处理 `WM_MOUSE*`、`WM_KEY*`、`WM_CHAR`，没有 `WM_POINTER`、`EnableMouseInPointer`、`WM_GESTURE`/`SetGestureConfig`。普通触摸会被 Windows 提升为鼠标消息，基本点击/拖动可能可用，但压力、接触 id、多指缩放/旋转与笔 hover 不可用；需确认产品是否要求触控图表手势。Chrome 在 `hwnd_message_handler.cc` 中完整处理 `WM_POINTER`（含 pen/touch 区分）。
- **深色标题栏**：`WM_SETTINGCHANGE` 的 `ImmersiveColorSet` 只产生 `PlatformEvent::Appearance`（`event_loop.rs:542-588`），没有任何 `DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE)` 调用。若系统标题栏保持浅色而内容为深色，需实测视觉一致性；Chrome 在 `allow_dark_mode_for_window`/DWM 属性上做了配套。
- **UIA 客户端在窗口销毁过程中的请求**：`WndProc` 没有像 Chromium 那样在 `WM_DESTROY` 时关闭 `WM_GETOBJECT` 服务（`a11y.rs` 无 `WM_DESTROY` 处理）。AccessKit `Adapter` 的 Drop 行为需要在目标 `accesskit_windows 0.35.2` 上确认；建议用屏幕阅读器在关闭窗口时压力测试，确认不会在 `DestroyWindow` 期间返回 provider。
- **`DCompositionCreateDevice2(None)` 的设备丢失处理**：DirectComposition 官方最佳实践建议在 `WM_PAINT` 中调用 `IDCompositionDevice::CheckDeviceState`；当前 `WM_PAINT` 只 `BeginPaint/EndPaint` 并 `wake`（`event_loop.rs:363-371`），没有检查 DComp 设备状态。由于内容全部走交换链且渲染器已有 `poll_renderers` 的设备丢失通道，是否会在 D3D12 设备丢失时同步失效需要实测；若需要，补 `CheckDeviceState` 并在 `WM_PAINT` 冒泡 `EngineError::Composition`。
- **双精度/高精度滚轮**：`event.rs:114-125` 直接把 `WM_MOUSEWHEEL` 的 ±120 倍数除以 120 得到 `f32` ticks；没有按 `WHEEL_DELTA` 累积小数余量。若应用层把每个事件当作固定行数滚动，精度触控板的小 delta 可能被放大/丢失；需结合 `on_scroll` 的 `scroll_to_animated` 实测滚动距离。
