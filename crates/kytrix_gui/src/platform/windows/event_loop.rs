//! Windows 事件循环：`WndProc`、消息泵、唤醒合并、窗口生命周期
//! （`design/phase-09-win32-demo.md` §5.2 / §5.4）。
//!
//! 两条实测约束（probe）：
//! 1. `PeekMessageW` 会**取出并消费** `WM_QUIT`，泵必须在取出后显式判 `WM_QUIT` 再退出，
//!    否则后续 `GetMessageW` 永久阻塞（DEVIATIONS F-90）。
//! 2. `WM_PAINT` 不验证会形成风暴（100 ms / 24,576 条，V-63）：本文件的 `WM_PAINT` 分支只做
//!    `BeginPaint`/`EndPaint`，不在这里绘制。
//!
//! `WndProc` **不做**任何 `App` 调用：它只把 [`PlatformEvent`] 压进 `WndProcState`，泵在
//! `DispatchMessageW` 返回后统一消费（Rust 借用规则 + 保持 `WndProc` 廉价）。

use std::sync::Arc;

use windows::Win32::{
    Foundation::{ERROR_SUCCESS, HWND, LPARAM, LRESULT, POINT, WPARAM},
    Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT, ScreenToClient},
    System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
    UI::{
        HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
        WindowsAndMessaging::*,
    },
};

use crate::{
    app::App,
    error::EngineError,
    event::{Event, InputEvent},
    platform::{
        dispatcher::{Dispatcher, WM_APP_FRAME_WAKE},
        event::{
            PlatformEvent, PointerKind, TranslateState, hiword_signed, loword_signed, translate,
        },
        windows::{dpi, window::NativeWindow},
    },
    render::backend::Renderer,
    window::{WindowId, WindowOptions},
};

/// `WndProc` 的每窗口状态（`GWLP_USERDATA` 指向它）。
pub(crate) struct WndProcState {
    dispatcher: Arc<Dispatcher>,
    events: Vec<PlatformEvent>,
    translate: TranslateState,
    /// `WM_GETMINMAXINFO` 的 `ptMinTrackSize`（物理像素）。
    min_size: (u32, u32),
    close_requested: bool,
    /// 最近一次指针位置（客户区物理像素）：IME 组合窗按它定位。
    cursor: (i32, i32),
}

impl WndProcState {
    pub(crate) fn new(dispatcher: Arc<Dispatcher>) -> Self {
        Self {
            dispatcher,
            events: Vec::with_capacity(8),
            translate: TranslateState::default(),
            min_size: (1, 1),
            close_requested: false,
            cursor: (0, 0),
        }
    }

    pub(crate) fn set_min_size(&mut self, min: (u32, u32)) {
        self.min_size = min;
    }

    fn wake(&self) {
        self.dispatcher.wake();
    }

    fn push(&mut self, event: PlatformEvent) {
        self.events.push(event);
    }

    pub(crate) fn take_events(&mut self) -> Vec<PlatformEvent> {
        std::mem::take(&mut self.events)
    }

    pub(crate) fn take_close_request(&mut self) -> bool {
        std::mem::take(&mut self.close_requested)
    }

    /// 当前修饰键（事件投递时随按键一起给应用层）。
    pub(crate) fn modifiers(&self) -> crate::event::Modifiers {
        self.translate.modifiers()
    }
}

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
        // 系统标题栏：非客户区完全交给默认过程（P09 不做自绘标题栏）。
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

        let state = &mut *state;
        if msg == WM_APP_FRAME_WAKE {
            // `Dispatcher::wake` 已置位，这里只需要返回。
            return LRESULT(0);
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
            // 指针位置用于 IME 组合窗定位（P09 没有 caret 语义，用最后指针位置）。
            if let PlatformEvent::Pointer { x, y, .. } = &event {
                state.cursor = (*x, *y);
            }
            if matches!(event, PlatformEvent::Ime(crate::event::ImeEvent::Start)) {
                state.translate.ime_mut().set_composition_point(
                    hwnd,
                    state.cursor.0,
                    state.cursor.1,
                );
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

fn is_ime_message(msg: u32) -> bool {
    matches!(
        msg,
        WM_IME_STARTCOMPOSITION | WM_IME_COMPOSITION | WM_IME_ENDCOMPOSITION
    )
}

/// `WM_SETTINGCHANGE` 且是「沉浸式颜色集」变化 → 读注册表拿深浅色。
fn appearance_change(lparam: isize) -> Option<PlatformEvent> {
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

/// 读 `HKCU\...\Themes\Personalize\AppsUseLightTheme`（1 = 浅色）。
fn dark_mode() -> Option<bool> {
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

/// 启动时声明 DPI 感知：per-monitor v2（`WM_DPICHANGED` 才带每屏 DPI）。
fn install_dpi_awareness() {
    // 失败通常意味着清单/宿主已经设过 awareness（返回 `ERROR_ACCESS_DENIED`）：沿用现有设置。
    // SAFETY: 无参数指针的进程级设置；必须在创建任何窗口之前调用。
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

/// 引擎自有事件循环：建窗口 → 挂渲染器 → 显示 → 泵 → 关闭。
pub(crate) fn run(mut app: App) -> Result<(), EngineError> {
    install_dpi_awareness();
    let dispatcher = app.dispatcher();

    let requests: Vec<(WindowId, WindowOptions)> = app
        .windows()
        .map(|(id, window)| (id, window.options().clone()))
        .collect();
    let mut windows: Vec<NativeWindow> = Vec::with_capacity(requests.len());
    for (id, options) in requests {
        let native = NativeWindow::create(id, &options, dispatcher.clone())?;
        let client = native.client_size();
        let renderer = Renderer::new_for_hwnd(native.hwnd_raw(), client)?;
        if let Some(window) = app.window_mut(id) {
            window.set_renderer(renderer);
            window.set_scale(native.scale());
            window.set_size(native.logical_size());
        }
        native.show();
        windows.push(native);
    }
    if let Some(first) = windows.first() {
        dispatcher.attach(first.hwnd_raw());
        app.set_native_window(first.hwnd_raw());
    }
    // 首帧：显示之后请求一帧（I3 的「空闲」只发生在没有 dirty 时）。
    dispatcher.wake();
    pump(app, windows, dispatcher)
}

fn pump(
    mut app: App,
    mut windows: Vec<NativeWindow>,
    dispatcher: Arc<Dispatcher>,
) -> Result<(), EngineError> {
    let mut message = MSG::default();
    loop {
        // ① 先 drain：`WM_QUIT` 必须显式识别（`PeekMessageW` 会把它取出来）。
        loop {
            // SAFETY: `message` 是栈上值；过滤范围 0..0 = 本线程全部消息。
            let has_message = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) };
            if !has_message.as_bool() {
                break;
            }
            if message.message == WM_QUIT {
                return Ok(());
            }
            // SAFETY: `message` 由 `PeekMessageW` 填充；`TranslateMessage` 可能产生 `WM_CHAR`。
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }

        // ② 平台事件 → App。
        collect_events(&mut app, &mut windows, &dispatcher)?;

        // ③ 关闭：关掉 App 里的窗口与原生窗口；一个不剩就退出。
        if close_requested_windows(&mut app, &mut windows)? {
            return Ok(());
        }

        // ④ 应用钩子（读事件 / 写状态），再提交本帧；两段门禁在 `App::frame` 里，闲时是 `Idle`。
        app.run_frame_hook();
        if dispatcher.take_frame_wake() {
            frame_all(&mut app)?;
        }

        // ⑤ 跨线程回投的任务。
        for task in dispatcher.drain_tasks() {
            task.run(&mut app);
        }

        // ⑥ 阻塞等待（I3）：没有消息时停在这里。
        // SAFETY: 同 ①。
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 == 0 {
            return Ok(());
        }
        if result.0 == -1 {
            return Err(EngineError::WindowCreate {
                detail: smol_str::format_smolstr!(
                    "GetMessageW: GetLastError={}",
                    unsafe { windows::Win32::Foundation::GetLastError() }.0
                ),
            });
        }
        // SAFETY: `message` 已由 `GetMessageW` 填充。
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// 把 `WndProc` 收集的事件应用到 `App`（几何/DPI 先落地，再投递事件）。
fn collect_events(
    app: &mut App,
    windows: &mut [NativeWindow],
    dispatcher: &Arc<Dispatcher>,
) -> Result<(), EngineError> {
    for native in windows.iter_mut() {
        let id = native.id();
        let events = native.state_mut().take_events();
        for event in events {
            apply_event(app, native, dispatcher, id, event)?;
        }
    }
    Ok(())
}

fn apply_event(
    app: &mut App,
    native: &mut NativeWindow,
    dispatcher: &Arc<Dispatcher>,
    id: WindowId,
    event: PlatformEvent,
) -> Result<(), EngineError> {
    let scale = native.scale();
    let to_logical = |x: i32, y: i32| {
        crate::math::Point::new(scale.to_logical(x as f32), scale.to_logical(y as f32))
    };
    let app_event = match event {
        PlatformEvent::Pointer { x, y, kind } => {
            let position = to_logical(x, y);
            let input = match kind {
                PointerKind::Move => InputEvent::PointerMove { position },
                PointerKind::Down(button) => InputEvent::PointerDown { position, button },
                PointerKind::Up(button) => InputEvent::PointerUp { position, button },
            };
            Event::Input(input)
        }
        PlatformEvent::Scroll {
            x,
            y,
            dx_ticks,
            dy_ticks,
        } => Event::Input(InputEvent::Scroll {
            position: to_logical(x, y),
            delta_x: dx_ticks,
            delta_y: dy_ticks,
        }),
        PlatformEvent::Key { key, state } => Event::Input(InputEvent::Key {
            key,
            state,
            modifiers: native.state_mut().modifiers(),
        }),
        PlatformEvent::Text(text) => Event::Input(InputEvent::Text { text }),
        PlatformEvent::Ime(ime) => Event::Ime(ime),
        PlatformEvent::Resize { width, height } => {
            native.note_client_resize((width, height));
            let size = native.logical_size();
            if let Some(window) = app.window_mut(id) {
                window.set_size(size);
            }
            dispatcher.wake();
            Event::Resized { size }
        }
        PlatformEvent::Dpi { dpi, suggested } => {
            let scale = dpi::scale_from_dpi(dpi).ok_or(EngineError::InvalidDpiScale {
                factor: dpi as f32 / dpi::BASE_DPI as f32,
            })?;
            native.apply_scale(scale, dpi)?;
            if let Some(rect) = suggested {
                native.place(rect)?;
            }
            if let Some(window) = app.window_mut(id) {
                window.set_scale(scale);
            }
            dispatcher.wake();
            Event::DpiChanged { dpi, scale }
        }
        PlatformEvent::Focus(focused) => Event::FocusChanged { focused },
        PlatformEvent::Appearance(dark) => Event::AppearanceChanged { dark },
        PlatformEvent::Close => Event::CloseRequested,
    };
    app.dispatch(id, app_event);
    Ok(())
}

/// 关闭被要求关闭的窗口；返回 `true` = 没有窗口了（泵退出）。
fn close_requested_windows(
    app: &mut App,
    windows: &mut Vec<NativeWindow>,
) -> Result<bool, EngineError> {
    let mut closed = false;
    for native in windows.iter_mut() {
        if native.state_mut().take_close_request() {
            let id = native.id();
            app.close_window(id);
            native.destroy();
            closed = true;
        }
    }
    if closed {
        windows.retain(|native| !native.is_destroyed());
    }
    Ok(windows.is_empty() || app.windows().next().is_none())
}

/// 逐窗口提交一帧；`Err` 立刻终止（device lost / swapchain 失败都不可重试）。
fn frame_all(app: &mut App) -> Result<(), EngineError> {
    let ids: Vec<WindowId> = app.windows().map(|(id, _)| id).collect();
    for id in ids {
        match app.frame(id) {
            None | Some(Ok(_)) => {}
            Some(Err(error)) => return Err(error),
        }
    }
    Ok(())
}

fn encode_point(x: i32, y: i32) -> isize {
    ((((y as u32) << 16) | (x as u32 & 0xFFFF)) as i32) as isize
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
