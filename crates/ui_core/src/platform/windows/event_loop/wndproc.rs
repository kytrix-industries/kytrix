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
//! 2. **非客户区全部交系统**（R07-2）：`WM_NCCALCSIZE` / `WM_NCLBUTTONDOWN` 不改写，
//!    `WM_NCHITTEST` 也只在**自绘标题栏拖动区**（X2，`drag_regions` 命中）时返回
//!    `HTCAPTION`，其余仍交 `DefWindowProcW`。命中码、边框几何、缩放光标，以及 caption 的
//!    move 模态循环与 8 个命中码的 size 模态循环都由系统提供（Aero Snap、Win11 Snap
//!    Layouts、双击边框、`Alt+Space` → 大小、拖动中跨屏 DPI 全部免费；Chrome / Qt / GPUI
//!    同款）。拖动区返回 `HTCAPTION` 走的就是同一条系统 move 模态循环。
//!
//!    size 模态循环期间泵的 ④ 不运行，但重排 + 重绘没有停：系统把 `WM_SIZE` 同步分派到本
//!    函数，`WM_ENTERSIZEMOVE` 之后每一条 `WM_SIZE` 都由
//!    [`super::resize::ResizeFrameDriver`] 直接驱动一帧真实帧（与 Qt 在 `WM_SIZE` 里同步
//!    布局 / 重绘、Chrome 在每个 size 步 `SetScaleAndSize` 之后合成同构）。
//!
//!    **move 模态循环（拖动标题栏，B1）没有 `WM_SIZE`**：它唯一的出帧入口是
//!    `WM_APP_FRAME_WAKE`（tokio 回投 / 行情写入 / 帧节拍都只 `Dispatcher::wake`）。泵的 ④ 同样
//!    停摆，所以本函数在**全局** `Dispatcher::in_modal_loop()` 为真时把这条消息变成一步
//!    [`super::resize::ModalTick`]（drain 任务 → `run_frame_hook` → 全部窗口 `App::frame`）。
//!    不看本窗口的 `size_move`：唤醒目标是 attach 首窗，被拖窗口不一定是它。模态循环外这条
//!    消息**保持原语义**（只返回，由泵的 ④ 统一出帧）。

use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT, ScreenToClient},
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
use crate::{
    math::Point,
    platform::{
        dispatcher::{WM_APP_COMPOSITION_WAKE, WM_APP_FRAME_WAKE},
        event::{PlatformEvent, StampedEvent, hiword_signed, loword_signed, translate},
        windows::{composition, dark_mode, dpi, ime},
    },
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
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WndProcState;
        if state.is_null() {
            // 创建期（尚未绑定状态）：没有装饰模式可读，交默认过程。
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        // ── E6 frameless：客户区铺满整个窗口（§11.6）─────────────────────────────
        //
        // `Decorations::Native` 完全交默认过程（既有行为逐字节不变）。
        //
        // `Decorations::None`：wparam == TRUE 时**返回 0** = 「客户区已由应用算好」—— 不修改
        // `rgrc[0]` 就是客户区 = 整个窗口矩形（去掉系统标题栏 / 边框的非客户区绘画）。窗口样式
        // 仍带 `WS_THICKFRAME` / `WS_CAPTION`（见 `NativeWindow::create`），因此 **DWM 阴影 /
        // 圆角 / 最大化动画 / Aero Snap 全部保留**（`platform/windows/chrome` 模块文档）：
        // 不换成 `WS_POPUP` 是本节唯一的架构前提。
        //
        // 最大化补偿：系统把最大化窗口的窗口矩形撑到「工作区 + 一圈边框」（装饰窗口的 frame
        // 厚度），客户区若仍等于窗口矩形，内容会在屏幕四边被裁掉 1–8 px。按与 `WM_NCHITTEST`
        // 命中带**同一个** `resize_border` 向内缩，客户区正好等于工作区。
        //
        // wparam == FALSE（`lParam` 是窗口 RECT、不是 `NCCALCSIZE_PARAMS`）交默认过程：该变体只
        // 允许应用给出客户区矩形，frameless 不需要改写它（Raymond Chen 的 custom frame 系列
        // 同样只处理 TRUE）。20 年的 recipe 与本仓的差别只在这里的注释与测试。
        if msg == WM_NCCALCSIZE
            && wparam.0 != 0
            && (*state).decorations() == crate::window::Decorations::None
        {
            let params = &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS);
            if IsZoomed(hwnd).as_bool() {
                let border = (*state).resize_border();
                let current = params.rgrc[0];
                let inset = super::super::chrome::inset_for_maximized(
                    (current.left, current.top, current.right, current.bottom),
                    border,
                );
                params.rgrc[0] = windows::Win32::Foundation::RECT {
                    left: inset.0,
                    top: inset.1,
                    right: inset.2,
                    bottom: inset.3,
                };
            }
            return LRESULT(0);
        }

        // 非客户区命中：`Decorations::Native` 用**系统命中码**（`HTCAPTION` / `HTLEFT` /
        // `HTBOTTOMRIGHT` / `HTCLOSE`…）：命中码、边框几何、缩放光标形状都由系统算。
        // `WM_NCLBUTTONDOWN` 也不改写（R07-2）：caption 的 move 模态循环（Aero Snap（拖边）/
        // 双击最大化 / 右键系统菜单 / `Alt+Space`）与 8 个命中码的 size 模态循环都只存在于
        // `DefWindowProcW` 内部。Win11 Snap Layouts 的**悬停浮层**需要 `HTMAXBUTTON`，本批未实现
        // （保持如实：`WM_NCHITTEST` 不返回 `HTMAXBUTTON`）。
        //
        // **X2 的唯一例外**（两种装饰模式都有）：自绘标题栏拖动区（应用通过
        // `Window::set_drag_regions` 声明，泵每轮同步到 [`WndProcState::drag_regions`]）命中时
        // 返回 `HTCAPTION` —— 与系统标题栏走同一条 move 模态循环（双击最大化/还原、Aero Snap
        // 拖边、`Alt+Space`、右键系统菜单全部成立；Snap Layouts 浮层除外，见上）；区域是
        // **逻辑客户区坐标**：屏幕物理坐标（`WM_NCHITTEST` 的 lParam）→ `ScreenToClient` →
        // 除以本消息时刻的 scale。
        //
        // **E6 frameless 的另一半**：引擎自算 8 向 resize 命中（`chrome::resize_edge_at`，
        // 按 DPI 缩放的 `SM_CXSIZEFRAME + SM_CXPADDEDBORDER`）并返回对应 `HT*`：frameless 下
        // 客户区已铺满，若交 `DefWindowProcW` 它只会返回 `HTCLIENT`，缩放光标与 size 模态循环
        // 全部消失。resize 带**优先于**拖动区（§8.6.2：应用声明的拖动区不得覆盖系统 resize
        // 边框）；最大化时没有可缩放的非客户区，直接进入拖动区 / `HTCLIENT` 判定。
        if msg == WM_NCHITTEST {
            let mut point = POINT {
                x: loword_signed(lparam.0),
                y: hiword_signed(lparam.0),
            };
            // `ScreenToClient` 只做坐标换算、不派发消息（不违反纪律 1）；`hwnd` 存活由外层
            // `unsafe` 块的 SAFETY 注释覆盖。
            if ScreenToClient(hwnd, &mut point).as_bool() {
                let frameless = (*state).decorations() == crate::window::Decorations::None;
                if frameless && !IsZoomed(hwnd).as_bool() {
                    // `GetClientRect` 只查询、不派发消息（同 `ScreenToClient`）。
                    let mut client = windows::Win32::Foundation::RECT::default();
                    if GetClientRect(hwnd, &mut client).is_ok()
                        && let Some(edge) = super::super::chrome::resize_edge_at(
                            (point.x, point.y),
                            (client.right - client.left, client.bottom - client.top),
                            (*state).resize_border(),
                        )
                    {
                        return LRESULT(super::super::chrome::hit_test_code(edge) as isize);
                    }
                }
                let scale = (*state).scale();
                let logical = Point::new(
                    scale.to_logical(point.x as f32),
                    scale.to_logical(point.y as f32),
                );
                if (*state)
                    .drag_regions()
                    .iter()
                    .any(|region| region.contains(logical))
                {
                    return LRESULT(HTCAPTION as isize);
                }
                if frameless {
                    // 客户区内部：必须是 `HTCLIENT`（frameless 下客户区铺满，不能再交默认过程 ——
                    // 那会把内容点当非客户区处理，吃掉输入）。
                    return LRESULT(HTCLIENT as isize);
                }
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }

        if msg == WM_GETMINMAXINFO {
            let info = &mut *(lparam.0 as *mut MINMAXINFO);
            // B3：应用配置的最小客户区（`WindowOptions::min_size`，缺省 = 引擎 320×200）
            // 换算成**窗口外框**的 `ptMinTrackSize`；物理像素随 DPI 同步（`WndProcState::note_dpi`）。
            info.ptMinTrackSize = POINT {
                x: (*state).min_track_size.0 as i32,
                y: (*state).min_track_size.1 as i32,
            };
            // 只有应用显式配置了 `max_size` 才覆盖：否则保留系统按屏幕算出的默认最大跟踪尺寸
            //（窗口可被拖出屏幕的最大值由系统决定，引擎不越权限制）。
            if let Some((width, height)) = (*state).max_track_size {
                info.ptMaxTrackSize = POINT {
                    x: width as i32,
                    y: height as i32,
                };
            }
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
            // C2：MS《Best practices for DirectComposition》—— 合成设备失效时 DirectComposition
            // 会投递 `WM_PAINT`，应用必须据此检查 `IDCompositionDevice::CheckDeviceState` 并在
            // 失效时新建设备 / 重建 visual tree。`WndProc` 里只推进无锁代际（DComp 调用可能同步
            // 重入消息泵，违反文件头纪律 1）；检查在泵的几何 pass、重建在 GPU 线程。
            composition::note_device_check();
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
                // B3：这次重新落位属于 DPI 路径（不是用户移动）：期间系统同步派发的
                // `WM_MOVE` 不派发 `Event::Moved`（公共契约：DPI 变化只产生 `DpiChanged`）。
                // 置位只覆盖 `SetWindowPos` 同步调用期间，之后的用户/程序移动照常派发。
                (*state).suppress_move = true;
                // SAFETY: `hwnd` 存活；系统建议矩形只改位置与尺寸，不激活窗口、不改 Z 序。
                // 可能重入 `WndProc`（`WM_WINDOWPOSCHANGED` / `WM_SIZE` / `WM_MOVE`）：此处
                // 不持有 `&mut`。
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    left,
                    top,
                    right - left,
                    bottom - top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                (*state).suppress_move = false;
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
            // 系统 size / move 模态循环结束：之后回到泵的常规出帧路径（最后一条 `WM_SIZE` 已
            // 驱动过一帧真实帧；B1 的全局模态计数随 `end_size_move` 一起退出一层）。
            (*state).end_size_move();
        }
        if matches!(msg, WM_SETFOCUS | WM_EXITMENULOOP | WM_EXITSIZEMOVE) {
            let events = (*state).a11y_mut().focus_changed(true);
            if let Some(events) = events {
                events.raise();
            }
        }
        if matches!(msg, WM_KILLFOCUS | WM_ENTERMENULOOP | WM_ENTERSIZEMOVE) {
            // IME 失焦对齐（B6/D6 三端统一语义）：窗口真的失去键盘焦点时，组合中补**恰好
            // 一条** `ImeEvent::End`。Win32 **不保证**失焦一定先派发
            // `WM_IME_ENDCOMPOSITION`（TSF / 第三方 IME 可以先丢焦点、稍后才结束上下文），
            // 不补的话应用会永久停在 inline preedit。组合态由 `ImeState` 记账：这里补过一条后
            // 状态清零，系统随后迟到的 `WM_IME_ENDCOMPOSITION` 不再重复投递。
            //
            // 应用侧 caret hint 由泵路径清（`native_window::apply_event` 的 `Focus(false)` 分支，
            // `WndProc` 不碰 `App`）。
            if msg == WM_KILLFOCUS {
                let ticks = message_ticks();
                if let Some(end) = (*state).translate.ime_mut().finish_composition() {
                    let scale = (*state).scale();
                    (*state).push_stamped_event(StampedEvent {
                        event: PlatformEvent::Ime(end),
                        ticks_ms: ticks,
                        scale,
                    });
                    (*state).wake();
                }
            }
            // 丢失键盘焦点时必须**复位瞬时输入状态**（P2-6）：
            //
            // 修饰键是按消息顺序维护的（按下置位、抬起清位）。失焦期间抬起消息不再到达本窗口，
            // 于是 Alt+Tab / 打开菜单 / 进入模态缩放后，用户实际已经松开的 Shift/Ctrl 会永久
            // 保持"按下"，之后所有输入都被当成组合键。代理对半高与 IME 待抑制码元也一并清掉
            // （否则失焦前半个代理会和失焦后的字符拼成乱码，残留抑制码元会吞掉真实输入）。
            (*state).reset_transient_input();
            if msg == WM_ENTERSIZEMOVE {
                // 系统 size / move 模态循环开始（泵的 ④ 这段不运行）：size 循环由 `WM_SIZE`
                // 直接驱动真实帧（`ResizeFrameDriver`），move 循环（B1）由 `WM_APP_FRAME_WAKE`
                // 驱动的全局模态 tick 出帧 —— 全局计数在 `begin_size_move` 里，因为唤醒目标
                // 可能是别的窗口。
                (*state).begin_size_move();
            }
            let events = (*state).a11y_mut().focus_changed(false);
            if let Some(events) = events {
                events.raise();
            }
        }

        if msg == WM_APP_FRAME_WAKE {
            // `Dispatcher::wake` 已置位。模态循环**外**只需要返回 —— 泵的 ④ 会取走唤醒位并
            // 出帧（这里不得抢跑，否则每次唤醒会被绘制两次）。
            //
            // 注意这条路径与模态循环入口的竞态：本消息在循环外被派发时唤醒位**没有**被消费；
            // 如果同一批消息里紧跟系统 move 循环的起点（鼠标按下），泵的 ④ 还来不及消费就被
            // 模态循环占住 —— 那一位会「已置位但队列里没有消息」，循环里再也不会有 tick。
            // 因此 `begin_size_move`（`WM_ENTERSIZEMOVE`）会消费并重投一条唤醒消息。
            //
            // 系统 move / size 模态循环里泵的 ④ 停摆（`DispatchMessageW` 被系统循环占住）：
            // 这时本函数直接跑一步模态 tick（B1）。判定用**全局**计数 `in_modal_loop()` 而不是
            // 本窗口的 `size_move` —— `wake` 的目标是 attach 首窗，被拖窗口不一定是它。
            //
            // 两阶段（文件头纪律 1）：短借用只取 `ModalTick`（克隆 `Weak<App>` / `Arc<Dispatcher>`），
            // `App::frame` / 任务 / 钩子全部在借用**之外**执行；错误重新取状态写回
            // `pending_error`，与 `WM_SIZE` 的模态帧路径同款（帧管线失败绝不能静默）。
            if (*state).dispatcher().in_modal_loop()
                && let Some(step) = (*state).modal_tick_step()
                && let Some(error) = step.run()
            {
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WndProcState;
                if !state.is_null() {
                    (*state).note_pending_error(error);
                }
            }
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
            // B2：沉浸式颜色集变化 ⇒ 重新读注册表主题并同步标题栏明暗。`DwmSetWindowAttribute`
            // 只改 DWM 属性、**不派发窗口消息**，因此可以在 `&mut WndProcState` 的短借用内执行
            //（文件头纪律 1 只约束会同步重入本函数的调用）。
            appearance_dark(lparam_value).map(|dark| {
                let applied = dark_mode::set_titlebar(hwnd, dark);
                #[cfg(test)]
                {
                    // 测试观察点（生产构建没有该字段）：记录 DWM 调用**真的发生且成功**的明暗；
                    // 删掉上面的调用时恒为 `None`，单测据此变红。
                    (*state).last_dark_titlebar = applied.then_some(dark);
                }
                // 生产构建没有观察者：失败已在 `set_titlebar` 内部 `debug_assert`，窗口继续可用。
                #[cfg(not(test))]
                let _ = applied;
                StampedEvent {
                    event: PlatformEvent::Appearance(dark),
                    ticks_ms: ticks,
                    scale: (*state).scale(),
                }
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
            // B3：`WM_DPICHANGED` 建议矩形的重新落位是 DPI 路径的一部分 —— 期间系统同步派发的
            // `WM_MOVE` 不产生 `Event::Moved`（公共契约：该路径只产生 `DpiChanged`；见
            // `WndProcState::suppress_move`）。`WM_MOVE` 的默认过程返回 0，直接交默认过程。
            if (*state).suppress_move && matches!(stamped.event, PlatformEvent::Moved { .. }) {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
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
                // E6：最大化 / 还原 / Snap 必然改变客户区 ⇒ 本窗口的 chrome 状态（`IsZoomed`）
                // 需要在下一轮泵对账（`NativeWindow::sync_chrome`）。这里只置位：读 `IsZoomed` /
                // 写 `WindowState` 要碰 `App`，不能在窗口过程里做；本分支之前已经 `wake()`。
                (*state).note_chrome_dirty();
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
/// `WM_SETTINGCHANGE` 且是「沉浸式颜色集」变化 → 读注册表返回最新深色标志。
pub(super) fn appearance_dark(lparam: isize) -> Option<bool> {
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
    dark_mode::current()
}

/// 消费者：`wndproc` 的滚轮坐标翻译。
pub(super) fn encode_point(x: i32, y: i32) -> isize {
    ((((y as u32) << 16) | (x as u32 & 0xFFFF)) as i32) as isize
}
