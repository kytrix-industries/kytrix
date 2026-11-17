//! `WndProc` 消息分派：输入 / IME / DPI / a11y / 捕获 / `WM_NCHITTEST` 各分支。
//! `wndproc` 由 `NativeWindow::create` 注册为窗口过程（零行为变更的结构拆分）。

use windows::Win32::{
    Foundation::{ERROR_SUCCESS, HWND, LPARAM, LRESULT, POINT, WPARAM},
    Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT, ScreenToClient},
    System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
    UI::{
        Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE},
        WindowsAndMessaging::*,
    },
};

use super::{
    WndProcState,
    caption::{CAPTION_TIMER_ID, RESIZE_TIMER_ID, is_caption_hit, is_resize_hit},
    pump::{FRAME_TIMER_ID, cursor_handle},
};
use crate::platform::{
    dispatcher::{WM_APP_COMPOSITION_WAKE, WM_APP_FRAME_WAKE},
    event::{PlatformEvent, hiword_signed, loword_signed, translate},
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
    // 并在 `NativeWindow::drop` 之前清空（销毁后不会再有消息）。
    unsafe {
        // 非客户区仍由系统**绘制**（P09 不做自绘标题栏）：`WM_NCCALCSIZE` / `WM_NCHITTEST`
        // 都交给默认过程，最小化/最大化/关闭按钮、边框命中与 DPI 下的标题栏高度继续由系统
        // 负责；只有「按下后会让默认过程进入模态循环」的拖动/缩放由本过程接管（见下方
        // `WM_NCLBUTTONDOWN` 分支）。
        if msg == WM_NCCALCSIZE {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WndProcState;

        if msg == WM_GETMINMAXINFO {
            if !state.is_null() {
                let info = &mut *(lparam.0 as *mut MINMAXINFO);
                info.ptMinTrackSize = POINT {
                    x: (*state).min_size.0 as i32,
                    y: (*state).min_size.1 as i32,
                };
            }
            return LRESULT(0);
        }
        // 客户区的光标由引擎决定（P2-3）：每次鼠标移动系统都会问一次，不接管就会被
        // `DefWindowProcW` 重置成类光标（恒 `IDC_ARROW`），自定义光标永不生效。
        // 返回 1 = 「已处理，别再改」；非客户区（边框）交给系统，缩放箭头才对。
        if msg == WM_SETCURSOR {
            let hit = (lparam.0 as u32) & 0xFFFF;
            if hit == HTCLIENT && !state.is_null() {
                // SAFETY: 系统光标句柄由 `LoadCursorW` 借用、进程不拥有；`SetCursor`
                // 只影响当前线程。
                SetCursor(Some(cursor_handle((*state).cursor)));
                return LRESULT(1);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        // 引擎自己画：抑制系统擦背景（否则拖动/重绘时闪白）。
        if msg == WM_ERASEBKGND {
            return LRESULT(1);
        }
        if msg == WM_PAINT {
            let mut paint = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut paint);
            let _ = EndPaint(hwnd, &paint);
            if !state.is_null() {
                (*state).wake();
            }
            return LRESULT(0);
        }
        if state.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        // 应用主动禁用了本窗口的 IME（P2-7，自绘搜索框的英文直输等）：输入上下文已经从窗口
        // 上摘下，所有 `WM_IME_*` 一律吞掉 —— 不派发 `ImeEvent`、不写 caret 表单，也不交给
        // `DefWindowProcW`（它可能重新初始化/显示组合窗）。普通 `WM_CHAR` / 快捷键不受影响。
        //
        // win-1 P3：IMM32 在焦点 / 输入法切换后可能把输入上下文**重新关联**回窗口
        //（`WM_IME_SETCONTEXT` 就是「上下文关联状态变了」的通知，`WM_INPUTLANGCHANGE` 则是
        // 输入法本身变了）。这时只吞消息不够：上下文已经挂回去了，字母键会重新被 IME 吃成
        // 组合输入。先幂等再摘一次，再把消息吞掉。
        if !(*state).ime_enabled() && (is_any_ime_message(msg) || msg == WM_INPUTLANGCHANGE) {
            if msg == WM_IME_SETCONTEXT || msg == WM_INPUTLANGCHANGE {
                (*state).reassert_disabled_ime(hwnd);
            }
            return LRESULT(0);
        }
        // 应用自己持有的指针捕获被系统收回（Alt+Tab、系统弹窗、别的窗口 SetCapture）：
        // 此时**不会**收到 `WM_LBUTTONUP`，拖拽会话会永久停在「按住」。投递一次
        // `PointerCancel` 让交互层作废本次会话（结束拖拽 + 派发 `DragPhase::Cancel`）。
        // 自绘边框缩放 / 标题栏拖动有自己的收尾逻辑，走下面的分支，不重复投递。
        if msg == WM_CAPTURECHANGED && (*state).resize.is_none() && (*state).caption_drag.is_none()
        {
            if (*state).suppress_capture_cancel {
                // 引擎自己刚 `ReleaseCapture`（正常拖拽结束 / 缩放结束）：这不是会话丢失。
                // MSDN 明确「自己调用也会收到该消息」，不区分就会在每次松手后误投一次
                // `PointerCancel`，把 hover 清掉并给观察者注入幽灵事件。
                (*state).suppress_capture_cancel = false;
                return LRESULT(0);
            }
            (*state).events.push(PlatformEvent::PointerCancel);
            (*state).wake();
            return LRESULT(0);
        }

        // ── 自绘非客户区交互（缩放 + 标题栏拖动，P2-9） ──────────────────────────
        // 非客户区的缩放/移动不能交给 `DefWindowProcW`：`WM_NCLBUTTONDOWN` 的缩放分支会进入
        // Win32 的模态 sizing 循环、`HTCAPTION` 会进入 move 模态循环，把我们的泵阻塞在里面，
        // `WM_SIZE`/`WM_TIMER` 只能排队、画面在拖动期间不更新。这里接管按下/移动/抬起，普通
        // 消息循环继续跑，下一轮泵就能出帧。
        //
        // 系统标题栏**仍然由系统绘制**（`WM_NCCALCSIZE` / `WM_NCHITTEST` 交默认过程），所以
        // 这里只接管两个「按下后需要进入模态循环」的命中区域：缩放边框 → 自绘缩放；
        // `HTCAPTION` / `HTSYSMENU` → 自绘拖动。最小化/最大化/关闭按钮（`HTMINBUTTON` 等）
        // 继续走 `DefWindowProcW`，行为不变；它们不进入模态循环。
        {
            let state = &mut *state;
            let hit = wparam.0 as u32;
            if is_caption_hit(hit) {
                match msg {
                    WM_NCLBUTTONDOWN => {
                        // win-2 P3：UI 线程卡顿 / 仍持有捕获时，系统可能不投递
                        // `WM_NCLBUTTONDBLCLK`，只再来一条 `WM_NCLBUTTONDOWN`。自计时判定命中
                        // 就按双击处理（最大化/还原切换），否则开始拖动。
                        if state.self_timed_double_click(hwnd) {
                            state.toggle_maximize(hwnd);
                        } else {
                            state.begin_caption_drag(hwnd);
                        }
                        return LRESULT(0);
                    }
                    // 双击标题栏：系统行为是最大化/还原切换；自己 `ShowWindow`，
                    // 不让 `DefWindowProcW` 进入 move 模态循环。
                    WM_NCLBUTTONDBLCLK => {
                        state.toggle_maximize(hwnd);
                        return LRESULT(0);
                    }
                    // 引擎不提供系统菜单（见 `WindowOptions` 文档）：右键标题栏交给
                    // `DefWindowProcW` 会调用 `TrackPopupMenu` 的模态循环，直接吞掉。
                    WM_NCRBUTTONDOWN | WM_NCRBUTTONUP | WM_NCRBUTTONDBLCLK => return LRESULT(0),
                    _ => {}
                }
            }
            // Alt+Space（或标题栏图标的键盘等价）：默认过程会打开系统菜单的模态循环。
            if msg == WM_SYSCOMMAND && (wparam.0 as u32 & 0xFFF0) == SC_KEYMENU {
                return LRESULT(0);
            }
            if msg == WM_NCLBUTTONDOWN && is_resize_hit(hit) {
                state.begin_resize(hwnd, hit);
                return LRESULT(0);
            }
            if state.caption_drag.is_some() {
                match msg {
                    // 位置由 1 ms 合并定时器统一 `SetWindowPos`（同 P1-2b）：拖动期间每个
                    // `WM_MOUSEMOVE` 都移动会产生远高于刷新率的窗口重排。捕获期间系统只发
                    // 客户区形式（MSDN：捕获后 `WM_NCMOUSEMOVE` 不再发出）；非客户区形式
                    // 仅作防御（捕获没拿到 / 被系统提前收回）。
                    WM_MOUSEMOVE | WM_NCMOUSEMOVE | WM_MOVING => return LRESULT(0),
                    WM_TIMER if wparam.0 == CAPTION_TIMER_ID => {
                        state.apply_caption_drag(hwnd);
                        return LRESULT(0);
                    }
                    // `Esc` 取消拖动（win-2 P3）：与原生 move 循环一致 —— 窗口回到按下时的
                    // 矩形/最大化状态，按键不投递给应用。
                    WM_KEYDOWN if wparam.0 == VK_ESCAPE.0 as usize => {
                        state.cancel_caption_drag(hwnd);
                        return LRESULT(0);
                    }
                    // 捕获期间抬起是客户区形式（`WM_LBUTTONUP`），非客户区形式一并收尾，
                    // 避免拖动会话停在「按住」。
                    WM_LBUTTONUP | WM_NCLBUTTONUP => {
                        state.apply_caption_drag(hwnd);
                        state.end_caption_drag(hwnd);
                        return LRESULT(0);
                    }
                    // 右键取消拖动（win-2 P3）：原生 move 循环下右键会打开系统菜单并结束
                    // 移动；引擎不提供系统菜单（见 `WindowOptions` 文档），因此按「取消」处理。
                    // 捕获后这类消息是**客户区**形式，客户区坐标相对被拖动的窗口是负的，
                    // 交给应用会派发到错误位置。
                    WM_RBUTTONDOWN | WM_RBUTTONUP => {
                        state.cancel_caption_drag(hwnd);
                        return LRESULT(0);
                    }
                    // 中键 / 滚轮在拖动期间一律吞掉（同样避免坐标错乱的应用事件）。
                    WM_MBUTTONDOWN | WM_MBUTTONUP | WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                        return LRESULT(0);
                    }
                    WM_CAPTURECHANGED | WM_CANCELMODE => {
                        // 捕获被系统收回（Alt+Tab / 系统弹窗 / 别的窗口 SetCapture）：按最新
                        // 位置收尾，不留半移动的窗口。
                        state.apply_caption_drag(hwnd);
                        state.end_caption_drag(hwnd);
                        return LRESULT(0);
                    }
                    _ => {}
                }
            }
            if state.resize.is_some() {
                match msg {
                    // 只记录最新光标：`SetWindowPos` 由定时器 tick / 抬起 / 失去捕获合并（P1-2b）。
                    WM_MOUSEMOVE => {
                        state.note_resize_cursor();
                        return LRESULT(0);
                    }
                    WM_TIMER if wparam.0 == RESIZE_TIMER_ID => {
                        state.apply_resize(hwnd);
                        // 每个 tick 至多一帧：泵的 ②b′ 会按最终客户区重算几何。
                        state.wake();
                        return LRESULT(0);
                    }
                    WM_LBUTTONUP => {
                        state.note_resize_cursor();
                        state.apply_resize(hwnd);
                        state.end_resize(hwnd);
                        state.wake();
                        return LRESULT(0);
                    }
                    WM_CAPTURECHANGED => {
                        // 捕获被系统收回（Alt+Tab / 弹出菜单等）：用最新位置收尾，不留半移动的窗口。
                        state.note_resize_cursor();
                        state.apply_resize(hwnd);
                        state.end_resize(hwnd);
                        return LRESULT(0);
                    }
                    _ => {}
                }
            }
        }

        // ── a11y（P11）：借用纪律 ──────────────────────────────────────────────
        // `UiaReturnRawElementProvider`（转 `LRESULT` 时）与 `QueuedEvents::raise` 都会**同步**
        // 派发嵌套 `WM_GETOBJECT`。所以这两处只能在**不持有 `&mut WndProcState`** 的窗口里做：
        // 短借用块取出「未转换的值」，出了块再 `.into()` / `raise()`（P11 审查 #5）。
        if msg == WM_GETOBJECT {
            let pending = {
                // SAFETY: `state` 非空且由本线程独占（`GWLP_USERDATA` 的既有纪律）。
                let cell = &mut *state;
                if !cell.a11y_mut().begin_getobject() {
                    // 重入：本次不接管（不让 UIA 拿到半个 provider）。
                    return LRESULT(0);
                }
                cell.a11y_mut().wm_getobject(wparam, lparam)
            };
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
        if matches!(msg, WM_SETFOCUS | WM_EXITMENULOOP | WM_EXITSIZEMOVE) {
            // SAFETY: 短借用；`raise` 在借用释放之后（它可能派发嵌套消息）。
            let events = { (*state).a11y_mut().focus_changed(true) };
            if let Some(events) = events {
                events.raise();
            }
        }
        if matches!(msg, WM_KILLFOCUS | WM_ENTERMENULOOP | WM_ENTERSIZEMOVE) {
            // 丢失键盘焦点时必须**复位瞬时输入状态**（P2-6）：
            //
            // 修饰键是按消息顺序维护的（按下置位、抬起清位）。失焦期间抬起消息不再到达本窗口，
            // 于是 Alt+Tab / 打开菜单 / 进入模态缩放后，用户实际已经松开的 Shift/Ctrl 会永久
            // 保持"按下"，之后所有输入都被当成组合键。代理对半高也一并清掉（否则失焦前半个
            // 代理会和失焦后的字符拼成乱码）。
            (*state).reset_transient_input();
            // SAFETY: 同上。
            let events = { (*state).a11y_mut().focus_changed(false) };
            if let Some(events) = events {
                events.raise();
            }
        }

        let state = &mut *state;
        if msg == WM_APP_FRAME_WAKE {
            // `Dispatcher::wake` 已置位，这里只需要返回。
            return LRESULT(0);
        }
        if msg == WM_APP_COMPOSITION_WAKE {
            // GPU 线程在帧**完成**后通知「交换链里的内容尺寸变了」。这个通知**不请求新帧**：
            // 消息本身已经把 `GetMessageW` 唤醒，泵的 ②b′ 几何 pass 每轮都按
            // `presented_surface` / 客户区重算（同值几何是 no-op）。若在这里 `state.wake()`，
            // 每次完成 present 都会额外产生一次 `App::frame`（HUD 下是真实的 paint + present）。
            return LRESULT(0);
        }
        if msg == WM_TIMER {
            // 连续帧 tick：只唤醒；动画推进在 `App::frame` 的 `tick_animations`，
            // P17 HUD 的采样/请求 damage 在 `App::frame` 的 `prepare_debug_hud`。
            if wparam.0 == FRAME_TIMER_ID {
                state.wake();
                return LRESULT(0);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        if msg == WM_MOUSEMOVE && !state.mouse_tracking {
            // `PointerLeave` 的生产者：鼠标离开客户区时系统发 `WM_MOUSELEAVE`（0x02A3）。
            let mut track = TRACKMOUSEEVENT {
                cbSize: core::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            // `track` 在栈上，`hwnd` 是本窗口；失败时下一帧再试。
            if TrackMouseEvent(&mut track).is_ok() {
                state.mouse_tracking = true;
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

        let event = if is_ime_message(msg) {
            state
                .translate
                .ime_mut()
                .handle(hwnd, msg, lparam_value)
                .map(PlatformEvent::Ime)
        } else if msg == WM_SETTINGCHANGE {
            appearance_change(lparam_value)
        } else {
            translate(msg, wparam_value, lparam_value, &mut state.translate)
        };

        if let Some(event) = event {
            if matches!(event, PlatformEvent::PointerLeave) {
                state.mouse_tracking = false;
            }
            let is_close = matches!(event, PlatformEvent::Close);
            state.push(event);
            state.wake();
            if is_close {
                // 不交给 `DefWindowProcW`：销毁由事件循环在关掉 `App` 里的窗口之后做。
                state.close_requested = true;
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
    if status != ERROR_SUCCESS {
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
