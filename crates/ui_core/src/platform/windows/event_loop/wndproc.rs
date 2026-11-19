//! `WndProc` 消息分派：输入 / IME / DPI / a11y / 捕获 / 非客户区。
//! `wndproc` 由 `NativeWindow::create` 注册为窗口过程。
//!
//! ## 两条不可违反的纪律
//!
//! 1. **不把会重入 `WndProc` 的调用放进 `&mut WndProcState` 的借用里**（R07-7）。
//!    `SetWindowPos` / `ShowWindow` / `DefWindowProcW`（`WM_NCLBUTTONDOWN` 会进模态循环）/
//!    `ImmAssociateContext*` / `ImmSet*Window` 都会**同步**派发消息或重新进入本函数；嵌套
//!    进入时会再取一次 `GWLP_USERDATA` 并构造 `&mut WndProcState` —— 两个活跃 `&mut` 是别名
//!    UB。因此会重入的调用一律放在**短借用之间**：先算出命令/纯数据，释放借用，执行，再
//!    短借用写回（IME 的 [`ime::ImeCommand`] 就是这个形状）。
//! 2. **非客户区全部交系统**（R07-2）：`WM_NCCALCSIZE` / `WM_NCHITTEST` /
//!    `WM_NCLBUTTONDOWN` 都不改写。命中码、边框几何、缩放光标，以及 caption 的 move 模态
//!    循环与 8 个命中码的 size 模态循环都由系统提供（Aero Snap、Win11 Snap Layouts、
//!    双击边框、`Alt+Space` → 大小、拖动中跨屏 DPI 全部免费；Chrome / Qt / GPUI 同款）。
//!
//!    size 模态循环期间泵的 ④ 不运行，但重排 + 重绘没有停：系统把 `WM_SIZE` 同步分派到本
//!    函数，`WM_ENTERSIZEMOVE` 之后每一条 `WM_SIZE` 都由
//!    [`super::resize::ResizeFrameDriver`] 直接驱动一帧真实帧（与 Qt 在 `WM_SIZE` 里同步
//!    布局 / 重绘、Chrome 在每个 size 步 `SetScaleAndSize` 之后合成同构）。

use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT, ScreenToClient},
    System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
    UI::{
        Input::KeyboardAndMouse::{
            GetKeyState, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_LBUTTON, VK_MBUTTON,
            VK_RBUTTON, VK_SPACE, VK_XBUTTON1, VK_XBUTTON2,
        },
        WindowsAndMessaging::*,
    },
};

use super::{
    WndProcState,
    pump::{A11Y_TIMER_ID, cursor_handle, message_ticks},
    recovery,
};
use crate::platform::{
    dispatcher::{WM_APP_COMPOSITION_WAKE, WM_APP_FRAME_WAKE},
    event::{PlatformEvent, StampedEvent, hiword_signed, loword_signed, translate},
    windows::{dpi, ime},
};

/// 消费者：`NativeWindow::create`（`window.rs` 注册窗口过程）。
/// 窗口过程。所有分支都先取 `GWLP_USERDATA`；创建期（尚未绑定状态）一律交 `DefWindowProcW`。
pub(crate) extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: `hwnd` 由系统在消息分发时给出；`GWLP_USERDATA` 只由本模块写入（`NativeWindow::create`），
    // 并在 `NativeWindow::drop` 之前清空（销毁后不会再有消息）。下面所有对 `state` 的访问都是
    // **短借用**：会重入 `WndProc` 的调用之前必须已经释放借用（见文件头纪律 1）。
    unsafe {
        // 客户区**自绘**（DComp 提供全部像素，没有系统重定向表面）：系统仍按系统边框计算
        // 客户区，因此这里必须交默认过程 —— 自己返回 0 等于要求引擎画标题栏与边框。
        if msg == WM_NCCALCSIZE {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        // 非客户区命中：**系统命中码**（`HTCAPTION` / `HTLEFT` / `HTBOTTOMRIGHT` / `HTCLOSE`…）。
        //
        // `WM_NCHITTEST` 不改写：命中码、边框几何、缩放光标形状都由系统算。`WM_NCLBUTTONDOWN`
        // 也不改写（R07-2）：caption 的 move 模态循环（Aero Snap / Snap Layouts / 双击最大化 /
        // 右键系统菜单）与 8 个命中码的 size 模态循环都只存在于 `DefWindowProcW` 内部。
        // 曾经把 `HTCAPTION` 接管的实现因此丢掉了全部系统手势（多显示器用户最常用的窗口布局
        // 方式）；曾经自持 8 个 resize 命中码的实现则要自己重实现整套平台语义（见 `resize.rs`
        // 的模块文档：现在改为「系统拥有交互，`WM_SIZE` 驱动真实帧」）。
        if msg == WM_NCHITTEST {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WndProcState;
        if state.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        if msg == WM_GETMINMAXINFO {
            let info = &mut *(lparam.0 as *mut MINMAXINFO);
            info.ptMinTrackSize = POINT {
                x: (*state).min_size.0 as i32,
                y: (*state).min_size.1 as i32,
            };
            return LRESULT(0);
        }
        // 客户区的光标由引擎决定（P2-3）：每次鼠标移动系统都会问一次，不接管就会被
        // `DefWindowProcW` 重置成类光标（恒 `IDC_ARROW`），自定义光标永不生效。
        // 返回 1 = 「已处理，别再改」；非客户区（边框）交给系统，缩放箭头才对。
        if msg == WM_SETCURSOR {
            let hit = (lparam.0 as u32) & 0xFFFF;
            if hit == HTCLIENT {
                SetCursor(Some(cursor_handle((*state).cursor)));
                return LRESULT(1);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        // 引擎自己画：抑制系统擦背景（否则重绘时闪白）。
        if msg == WM_ERASEBKGND {
            return LRESULT(1);
        }
        if msg == WM_PAINT {
            let mut paint = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut paint);
            let _ = EndPaint(hwnd, &paint);
            (*state).wake();
            return LRESULT(0);
        }
        // ── R07-14：显示 / 电源恢复 ────────────────────────────────────────────
        //
        // 显示器拓扑变化（`WM_DISPLAYCHANGE`）、工作区变化（`WM_SETTINGCHANGE` +
        // `SPI_SETWORKAREA`）与睡眠唤醒（`WM_POWERBROADCAST` 的两个 resume 通知）都会让
        // **引擎持有的显示派生状态**失效：每窗口 DPI、客户区、交换链关联的显示器。
        //
        // 这里只登记 + 唤醒（判定是纯函数 [`recovery::recovery_plan`]）：重新查询显示器、
        // 重建交换链、全量重绘都在泵的 ②a′ 与 GPU 线程执行 —— 放在 `WndProc` 里会违反
        // 文件头纪律 1（`SetWindowPos` / 合成调用都会重入本函数）。
        //
        // 三条消息都**不**翻译成 `PlatformEvent`：恢复是平台层内部动作，不是应用可见的输入。
        if recovery::recovery_plan(msg, wparam.0).is_some() {
            (*state).note_display_recovery();
            (*state).wake();
            match msg {
                // 电源广播的约定：返回 `TRUE` = 已处理（`MSDN power broadcast`）。恢复动作
                // 已经在上面登记，系统不会重发这条消息，所以这里必须应答。
                WM_POWERBROADCAST => return LRESULT(1),
                // 显示变化不交给默认过程：本窗口的全部像素由 DirectComposition 提供
                // （`WS_EX_NOREDIRECTIONBITMAP`），`DefWindowProcW` 没有可刷新的重定向表面。
                WM_DISPLAYCHANGE => return LRESULT(0),
                // 工作区变化继续走下面的外观色 / 滚轮行数路径（同一条消息也可能带着
                // `ImmersiveColorSet` 的 `lParam`）。
                _ => {}
            }
        }
        // 应用主动禁用了本窗口的 IME（自绘搜索框的英文直输等）：输入上下文已经从窗口上摘下，
        // 所有 `WM_IME_*` 一律吞掉 —— 不派发 `ImeEvent`、不写 caret 表单，也不交给
        // `DefWindowProcW`（它可能重新初始化/显示组合窗）。普通 `WM_CHAR` / 快捷键不受影响。
        //
        // IMM32 在焦点 / 输入法切换后可能把输入上下文**重新关联**回窗口（`WM_IME_SETCONTEXT`
        // 就是「上下文关联状态变了」的通知）。这时只吞消息不够：上下文已经挂回去了，字母键会
        // 重新被 IME 吃成组合输入。先幂等再摘一次（两阶段，见文件头纪律 1），再把消息吞掉。
        if !(*state).ime_enabled() && (is_any_ime_message(msg) || msg == WM_INPUTLANGCHANGE) {
            if msg == WM_IME_SETCONTEXT || msg == WM_INPUTLANGCHANGE {
                let command = (*state).plan_ime_reassert();
                if let Some(command) = command {
                    let applied = ime::execute(hwnd, command);
                    // 重新摘除**不覆盖**已保存的待恢复上下文（见 `ImeState::store_reasserted`）。
                    (*state).note_ime_reasserted(applied);
                }
            }
            return LRESULT(0);
        }
        // R07-3：引擎自己画 inline preedit（`ImeEvent::Preedit`），必须清掉 IME 自绘组合窗与
        // guideline，否则用户看到「输入的拼音出现两遍」；候选窗仍由 IME 画，位不变。
        if msg == WM_IME_SETCONTEXT {
            let masked = ime::mask_setcontext(wparam.0, lparam.0);
            return DefWindowProcW(hwnd, msg, wparam, LPARAM(masked));
        }
        // R07-12：`WM_DPICHANGED` **在这里同步生效**，不排队到整批消息派发之后。
        //
        // 顺序不能换：先把新 scale / DPI 记进 `WndProcState`（同批次后续消息的坐标换算与
        // `WM_GETMINMAXINFO` 都读它），再按系统建议矩形 `SetWindowPos`（winit / Qt 同样在消息
        // 处理内完成），最后才投递应用事件。`SetWindowPos` 会同步派发 `WM_SIZE`，它的
        // `StampedEvent` 因此带上**新** scale。
        if msg == WM_DPICHANGED {
            let dpi = (wparam.0 as u32) & 0xFFFF;
            let Some(scale) = dpi::scale_from_dpi(dpi) else {
                // 非法 DPI（0 / 非有限）：丢掉这条消息比按 1.0 重排更安全。
                return LRESULT(0);
            };
            let ticks = message_ticks();
            (*state).note_dpi(dpi, scale);
            let context = (*state).context(ticks);
            if let Some(stamped) =
                translate(msg, wparam.0, lparam.0, context, &mut (*state).translate)
            {
                (*state).push_stamped_event(stamped);
            }
            let suggested = (!lparam.0.is_negative() && lparam.0 != 0).then(|| {
                let rect = &*(lparam.0 as *const windows::Win32::Foundation::RECT);
                (rect.left, rect.top, rect.right, rect.bottom)
            });
            if let Some((left, top, right, bottom)) = suggested {
                // SAFETY: `hwnd` 存活；系统建议矩形只改位置与尺寸，不激活窗口、不改 Z 序。
                // 可能重入 `WndProc`（`WM_WINDOWPOSCHANGED` / `WM_SIZE`）：此处不持有 `&mut`。
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    left,
                    top,
                    right - left,
                    bottom - top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            (*state).wake();
            return LRESULT(0);
        }
        // 应用自己持有的指针捕获被系统收回（Alt+Tab、系统弹窗、别的窗口 SetCapture）：此时
        // **不会**收到 `WM_LBUTTONUP`，拖拽会话会永久停在「按住」。投递一次 `PointerCancel`
        // 让交互层作废本次会话（结束拖拽 + 派发 `DragPhase::Cancel`）。
        if msg == WM_CAPTURECHANGED {
            if (*state).suppress_capture_cancel {
                // 引擎自己刚 `ReleaseCapture`（正常拖拽结束）：这不是会话丢失。
                // MSDN 明确「自己调用也会收到该消息」，不区分就会在每次松手后误投一次
                // `PointerCancel`，把 hover 清掉并给观察者注入幽灵事件。
                (*state).suppress_capture_cancel = false;
                return LRESULT(0);
            }
            // 非客户区交系统之后（R07-2），原生 move/size 模态循环会自己 `SetCapture` 我们的
            // 窗口、结束时再 `ReleaseCapture`，于是每次拖动/缩放窗口结束都会送来一条
            // `WM_CAPTURECHANGED`。那不是「拖拽被抢走」：真正的会话丢失一定发生在**按键仍
            // 按下**时（Alt+Tab / 系统弹窗 / 别的窗口抢捕获），而模态循环收尾时按键已经抬起。
            // 用按键状态区分，不需要追踪 `WM_ENTERSIZEMOVE`/`WM_EXITSIZEMOVE` 的配对与顺序。
            let mut button_down = false;
            for key in [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2] {
                // SAFETY: `GetKeyState` 是无副作用的线程局部查询（高位置位 = 按下）。
                if GetKeyState(i32::from(key.0)) < 0 {
                    button_down = true;
                    break;
                }
            }
            if !button_down {
                return LRESULT(0);
            }
            (*state).push_now(PlatformEvent::PointerCancel);
            (*state).wake();
            return LRESULT(0);
        }

        // ── a11y（P11）：借用纪律 ──────────────────────────────────────────────
        // `UiaReturnRawElementProvider`（转 `LRESULT` 时）与 `QueuedEvents::raise` 都会**同步**
        // 派发嵌套 `WM_GETOBJECT`。所以这两处只能在**不持有 `&mut WndProcState`** 的窗口里做：
        // 短借用块取出「未转换的值」，出了块再 `.into()` / `raise()`（P11 审查 #5）。
        if msg == WM_GETOBJECT {
            if !(*state).a11y_mut().begin_getobject() {
                // R07-8：嵌套查询必须**正常应答**，不能返回 0。
                //
                // 返回值 0 的语义是「本对象没有 provider」，UIA 会判定整个窗口不支持
                // 无障碍（屏幕阅读器里界面变成空白窗口）。`&mut WndProcState` 在
                // `.into()` 之前就已释放，重入本身是安全的（与 R07-7 的 IME 路径不同），
                // 因此这里把嵌套请求交给默认过程（系统默认 provider），而不是拒绝服务。
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            let pending = (*state).a11y_mut().wm_getobject(wparam, lparam);
            let result = match pending {
                Some(value) => value.into(),
                None => DefWindowProcW(hwnd, msg, wparam, lparam),
            };
            // SAFETY: 转换已完成，此时没有其它 `&mut` 存活。
            (*state).a11y_mut().end_getobject();
            return result;
        }
        // 窗口焦点态同步（与 `TranslateState` 的 `PlatformEvent::Focus` 是两条独立的线：
        // 前者给 AT 看「窗口有没有焦点」，后者给引擎做键盘分发）。
        if msg == WM_EXITSIZEMOVE {
            // 系统 size 模态循环结束：之后回到泵的常规出帧路径（最后一条 `WM_SIZE` 已驱动
            // 过一帧真实帧）。
            (*state).end_size_move();
        }
        if matches!(msg, WM_SETFOCUS | WM_EXITMENULOOP | WM_EXITSIZEMOVE) {
            let events = (*state).a11y_mut().focus_changed(true);
            if let Some(events) = events {
                events.raise();
            }
        }
        if matches!(msg, WM_KILLFOCUS | WM_ENTERMENULOOP | WM_ENTERSIZEMOVE) {
            // 丢失键盘焦点时必须**复位瞬时输入状态**（P2-6）：
            //
            // 修饰键是按消息顺序维护的（按下置位、抬起清位）。失焦期间抬起消息不再到达本窗口，
            // 于是 Alt+Tab / 打开菜单 / 进入模态缩放后，用户实际已经松开的 Shift/Ctrl 会永久
            // 保持"按下"，之后所有输入都被当成组合键。代理对半高与 IME 待抑制码元也一并清掉
            // （否则失焦前半个代理会和失焦后的字符拼成乱码，残留抑制码元会吞掉真实输入）。
            (*state).reset_transient_input();
            if msg == WM_ENTERSIZEMOVE {
                // 系统 size 模态循环开始：由 `WM_SIZE` 直接驱动真实帧（泵的 ④ 这段不运行）。
                (*state).begin_size_move();
            }
            let events = (*state).a11y_mut().focus_changed(false);
            if let Some(events) = events {
                events.raise();
            }
        }

        if msg == WM_APP_FRAME_WAKE {
            // `Dispatcher::wake` 已置位，这里只需要返回。
            return LRESULT(0);
        }
        if msg == WM_APP_COMPOSITION_WAKE {
            // GPU 线程在帧**完成**后通知「交换链里的内容尺寸变了」。这个通知**不请求新帧**：
            // 消息本身已经把 `GetMessageW` 唤醒，泵的 ②b′ 几何 pass 每轮都按
            // `presented_surface` / 客户区重算（同值几何是 no-op）。若在这里 `state.wake()`，
            // 每次完成 present 都会额外产生一次 `App::frame`（多数是空帧，但仍要跑完两段门禁）。
            return LRESULT(0);
        }
        if msg == WM_TIMER {
            // a11y 补发兜底：连续帧由帧节拍线程（`pump::FramePacer`）驱动，**不**用 `WM_TIMER`
            // （R07-10：`WM_TIMER` 是低优先级消息，输入密集时会被饿，且不与 DWM 节拍对齐）。
            if wparam.0 == A11Y_TIMER_ID {
                (*state).wake();
                return LRESULT(0);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        // Alt+Space（系统菜单的键盘等价）交给默认过程；其余 `SC_KEYMENU`（Alt+字母 的菜单
        // 激活）吞掉：引擎没有菜单栏，交给默认过程只会在部分配置下响一声提示音（P3-1）。
        if msg == WM_SYSCOMMAND && (wparam.0 as u32 & 0xFFF0) == SC_KEYMENU {
            let is_system_menu = lparam.0 == VK_SPACE.0 as isize;
            if !is_system_menu {
                return LRESULT(0);
            }
        }
        if msg == WM_MOUSEMOVE && !(*state).mouse_tracking {
            // `PointerLeave` 的生产者：鼠标离开客户区时系统发 `WM_MOUSELEAVE`（0x02A3）。
            let mut track = TRACKMOUSEEVENT {
                cbSize: core::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            // `track` 在栈上，`hwnd` 是本窗口；失败时下一帧再试。
            if TrackMouseEvent(&mut track).is_ok() {
                (*state).mouse_tracking = true;
                // R02-4：进入客户区的**第一个**消息就是 `PointerEnter` 的生产点（Win32 没有
                // 独立的 enter 消息，`TME_LEAVE` 只在离开时通知）。
                //
                // R02-14 / P2 回归：`PointerEnter` 的坐标由 `native_window::apply_event` 从
                // `last_client_pos` 读取，而它只在下面的 translate 结果里更新 —— 必须先记录
                // 本次移动的位置再 push，否则第一条 `PointerEnter.position` 是旧位置（首次为
                // `(0,0)`），并基于它命中一次 hover/光标，随后才被真正的 `PointerMove` 纠正。
                (*state).note_client_pos(loword_signed(lparam.0), hiword_signed(lparam.0));
                (*state).push_now(PlatformEvent::PointerEnter);
            }
        }

        // 滚轮：`lParam` 是屏幕坐标，翻译层要求客户区坐标。
        let (wparam_value, lparam_value) = (wparam.0, lparam.0);
        let lparam_value = if msg == WM_MOUSEWHEEL || msg == WM_MOUSEHWHEEL {
            let mut point = POINT {
                x: loword_signed(lparam_value),
                y: hiword_signed(lparam_value),
            };
            let _ = ScreenToClient(hwnd, &mut point);
            encode_point(point.x, point.y)
        } else {
            lparam_value
        };

        // R02-14：时间戳取 `MSG.time`（`GetMessageTime`），不是分发时刻。
        let ticks = message_ticks();
        // IME 单独一条路径：一条 `WM_IME_COMPOSITION` 最多产两条事件（`GCS_RESULTSTR |
        // GCS_COMPSTR` 同时置位 ⇒ Commit → Preedit），必须**逐条**入队（AR1b N5）。这条分支
        // 不产生 `Close`，因此不需要走下面的单事件收尾。
        if is_ime_message(msg) {
            // 组合串读取（`ImmGetContext` + `ImmGetCompositionStringW`）不派发消息，可以在借用
            // 内完成；**写**表单/上下文会重入，因此只产命令、出借用后再执行（文件头纪律 1）。
            // `ticks` 随消息进入 IME 状态：提交抑制队列用它做时间戳（AR3 §1.5）。
            let outcome = (*state)
                .translate
                .ime_mut()
                .handle(hwnd, msg, lparam_value, ticks);
            if let Some(command) = outcome.command {
                let applied = ime::execute(hwnd, command);
                (*state).note_ime_applied(applied);
            }
            if !outcome.events.is_empty() {
                let scale = (*state).scale();
                for event in outcome.events {
                    (*state).push_stamped_event(StampedEvent {
                        event: PlatformEvent::Ime(event),
                        ticks_ms: ticks,
                        scale,
                    });
                }
                (*state).wake();
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        let stamped = if msg == WM_SETTINGCHANGE {
            // 系统滚轮行数（`SPI_GETWHEELSCROLLLINES`）可能变了（R02-10）。
            (*state).refresh_wheel_lines();
            appearance_change(lparam_value).map(|event| StampedEvent {
                event,
                ticks_ms: ticks,
                scale: (*state).scale(),
            })
        } else {
            let context = (*state).context(ticks);
            translate(
                msg,
                wparam_value,
                lparam_value,
                context,
                &mut (*state).translate,
            )
        };

        if let Some(stamped) = stamped {
            match &stamped.event {
                PlatformEvent::PointerLeave => (*state).mouse_tracking = false,
                // `WM_MOUSELEAVE` 不带坐标：记下最近位置供 `PointerLeave` 的离开点使用。
                PlatformEvent::Pointer { x, y, .. } => (*state).note_client_pos(*x, *y),
                _ => {}
            }
            let is_close = matches!(stamped.event, PlatformEvent::Close);
            (*state).push_stamped_event(stamped);
            (*state).wake();
            // 系统 size 模态循环：`WM_SIZE` 先生效到窗口状态，然后**立刻**出一帧真实帧
            //（泵的 ④ 这段时间不运行；`WM_EXITSIZEMOVE` 之后回到常规路径，队列里的事件
            // 因同值幂等而不会重复出帧）。
            if msg == WM_SIZE {
                // `WM_SIZE` 的低 / 高 16 位 = 客户区物理宽 / 高（与 `TranslateState` 同源）。
                let raw = lparam_value as usize;
                let client = ((raw & 0xFFFF) as u32, ((raw >> 16) & 0xFFFF) as u32);
                // 两阶段（纪律 1）：短借用只取「这一步要驱动谁」，`App::frame` 在借用**之外**跑
                //（它的调用面很大，不能赌它不会重入 `WndProc`）。
                let step = (*state).resize_step(client);
                if let Some(step) = step
                    && let Some(error) = step.run()
                {
                    // 帧驱动期间窗口/状态不会被销毁（`App::frame` 不销毁窗口）：重新取一次状态
                    // 写回粘性错误，由泵的下一轮冒泡。
                    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WndProcState;
                    if !state.is_null() {
                        (*state).note_pending_error(error);
                    }
                }
            }
            if is_close {
                // 不交给 `DefWindowProcW`：销毁由事件循环在关掉 `App` 里的窗口之后做。
                (*state).close_requested = true;
                return LRESULT(0);
            }
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

/// 消费者：`wndproc` 的事件翻译。
pub(super) fn is_ime_message(msg: u32) -> bool {
    matches!(
        msg,
        WM_IME_STARTCOMPOSITION | WM_IME_COMPOSITION | WM_IME_ENDCOMPOSITION
    )
}

/// 消费者：`wndproc` 的 IME 禁用分支。
/// 全部 `WM_IME_*`（组合三连 + `IMM32` 的通知/字符/按键消息）。
///
/// 应用禁用 IME 时窗口一条都不处理：上下文虽然已经摘下，`IMM32` 在切换/重激活时仍可能送
/// 消息，交给 `DefWindowProcW` 会让它重新初始化输入上下文或显示组合窗。
pub(super) fn is_any_ime_message(msg: u32) -> bool {
    matches!(
        msg,
        WM_IME_STARTCOMPOSITION
            | WM_IME_COMPOSITION
            | WM_IME_ENDCOMPOSITION
            | WM_IME_SETCONTEXT
            | WM_IME_NOTIFY
            | WM_IME_CONTROL
            | WM_IME_COMPOSITIONFULL
            | WM_IME_SELECT
            | WM_IME_CHAR
            | WM_IME_REQUEST
            | WM_IME_KEYDOWN
            | WM_IME_KEYUP
    )
}

/// 消费者：`wndproc` 的 `WM_SETTINGCHANGE` 分支。
/// `WM_SETTINGCHANGE` 且是「沉浸式颜色集」变化 → 读注册表拿深浅色。
pub(super) fn appearance_change(lparam: isize) -> Option<PlatformEvent> {
    if lparam == 0 {
        return None;
    }
    // SAFETY: `lParam` 是系统给的以 NUL 结尾的宽字符串，只在本次消息处理期有效。
    let changed = unsafe {
        let text = lparam as *const u16;
        if text.is_null() {
            return None;
        }
        let mut len = 0usize;
        while *text.add(len) != 0 && len < 64 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(text, len);
        String::from_utf16_lossy(slice)
    };
    if changed != "ImmersiveColorSet" {
        return None;
    }
    dark_mode().map(PlatformEvent::Appearance)
}

/// 消费者：`appearance_change`。
/// 读 `HKCU\...\Themes\Personalize\AppsUseLightTheme`（1 = 浅色）。
pub(super) fn dark_mode() -> Option<bool> {
    let subkey = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let value = wide("AppsUseLightTheme");
    let mut data = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: 输出缓冲是栈上的 `u32`，`size` 同步给出字节数；键路径是 NUL 结尾的宽字符串。
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(subkey.as_ptr()),
            windows::core::PCWSTR(value.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut data as *mut u32).cast()),
            Some(&mut size),
        )
    };
    if status != windows::Win32::Foundation::ERROR_SUCCESS {
        return None;
    }
    Some(data == 0)
}

/// 消费者：`wndproc` 的滚轮坐标翻译。
pub(super) fn encode_point(x: i32, y: i32) -> isize {
    ((((y as u32) << 16) | (x as u32 & 0xFFFF)) as i32) as isize
}

/// 消费者：`dark_mode` 的注册表键名。
pub(super) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
