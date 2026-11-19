//! 原生窗口：创建（初始隐藏）/ 显示 / 客户区尺寸 / 最小尺寸 / 销毁
//!
//! 使用**系统标题栏**（`WS_OVERLAPPEDWINDOW`，`WM_NCCALCSIZE` 交 `DefWindowProcW`）；应用
//! 自绘标题栏时可以声明**拖动区**（X2 `Window::set_drag_regions`）：`WM_NCHITTEST` 命中区域
//! 返回 `HTCAPTION`，走系统 move 模态循环（拖动区之外的非客户区仍全部交系统）。
//!
//! 客户区**没有**系统的重定向表面（`WS_EX_NOREDIRECTIONBITMAP`）：窗口的全部像素由
//! [`Composition`] 合成的组合交换链提供。内容 visual 恒 1:1（resize 不缩放旧内容）；窗口放大
//! 露出的新区域由下方独立的背景层填充。

use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex},
};

use windows::Win32::{
    Foundation::{HWND, RECT},
    Graphics::Gdi::{
        DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, GetMonitorInfoW, HBRUSH,
        MONITOR_DEFAULTTONEAREST, MONITORINFO, MONITORINFOEXW, MonitorFromWindow,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{Input::KeyboardAndMouse::EnableWindow, WindowsAndMessaging::*},
};

use crate::{
    app::App,
    error::EngineError,
    math::{PhysicalRect, Rect, Scale, Size},
    platform::{
        dispatcher::Dispatcher,
        windows::{
            composition::{self, Composition},
            dark_mode, dpi,
            event_loop::{ResizeFrameDriver, WndProcState, wndproc},
            ime, placement,
        },
    },
    window::{WindowId, WindowOptions},
};

/// 窗口类名（每进程注册一次）。
const CLASS_NAME: &str = "KytrixGuiWindow";

/// `WindowOptions::position = None` 时的默认窗口外框左上角（物理像素）。
///
/// 保持 P09 以来的确定性默认落点 `(100, 100)`（`WindowOptions` 文档把 `None` 定义为「由平台
/// 决定」）：不改成 `CW_USEDEFAULT` 的系统级联位置 —— 固定值可预测、便于真机测试；多窗口的
/// 错落摆放由应用用 `position` 自己控制。
const DEFAULT_WINDOW_ORIGIN: (i32, i32) = (100, 100);

/// 类光标（`IDC_ARROW`）：probe 实测「类光标 = null + `HTCLIENT`」会得到**空光标**（不可见）。
fn load_arrow_cursor() -> HCURSOR {
    // SAFETY: 共享的系统光标句柄（进程不拥有）；`IDC_ARROW` 是静态常量。
    unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default()
}

/// `DEVMODEW.dmDisplayFrequency` → 帧预算的刷新率提示（纯函数，可单测）。
///
/// MS 文档：`dmDisplayFrequency` 为 **0** = 硬件默认、**1** = 显示器的默认/由驱动决定的速率 ——
/// 两者都不是可用的周期值，按「未知」返回 `None`。其余值原样透传，由
/// [`crate::window`] 的 `FrameBudget` 再做 24–480 Hz 的合法性校验（超范围同样禁用门禁）。
fn refresh_hz_from_display_frequency(frequency: u32) -> Option<f32> {
    (frequency > 1).then_some(frequency as f32)
}

/// 一个原生窗口（`Application::run` 持有；`Drop` 时销毁）。
pub(crate) struct NativeWindow {
    id: WindowId,
    hwnd: HWND,
    /// DirectComposition 合成目标：引用 HWND，因此 `destroy` 先 `shutdown` 它再销毁窗口。
    ///
    /// 与窗口的 `GpuRenderer` 共享（`Arc<Mutex<_>>`）：GPU 线程只在内容越过 buffer 时把 visual
    /// 切到新交换链（`GpuRenderer::grow`），其余 DComp 调用都在 UI 线程；互斥锁串行化全部调用。
    composition: Arc<Mutex<Composition>>,
    /// `GWLP_USERDATA` 指向它；`Drop` 时回收。
    state: *mut WndProcState,
    /// 最近一次成功应用到合成树的 `(presented, client)`。
    ///
    /// 泵的 ②b′ 只在它与「本轮的 presented / 客户区」不同、或 GPU 线程的兜底标记置位时才
    /// 碰 `Composition`（同值 `set_geometry` 本来也是 no-op，这里省掉每轮一次互斥锁 / 比较）。
    composition_geometry: Option<((u32, u32), (u32, u32))>,
    scale: Scale,
    logical_size: Size,
    /// 最近一次下发的 IME caret 区域（逻辑矩形 + 当时 Scale）。
    ime_area: Option<(Rect, Scale)>,
    /// 模态窗口打开期间被 `EnableWindow(owner, FALSE)` 禁用的 owner 窗口（X1）。
    ///
    /// 只有「`options.modal == true` 且 owner 存在」时才置位；销毁本窗口前必须恢复
    /// `EnableWindow(owner, TRUE)`，否则 owner 永久不可交互（系统级灰显 + 无输入）。
    /// owner 已被销毁时 `EnableWindow` 对失效句柄返回 FALSE（不 UB），无需特殊处理。
    modal_owner: Option<HWND>,
}

impl NativeWindow {
    /// 创建**隐藏**窗口并把客户区调到 `options.size`（逻辑像素 × 平台 DPI）。
    ///
    /// `owner` 是已解析的 owner 窗口句柄（X1）：`Some` ⇒ `CreateWindowExW` 的 `hWndParent`
    /// 走 owner 语义（不设 `WS_CHILD`，owned 窗口仍是顶层窗口）；`options.always_on_top` ⇒
    /// `WS_EX_TOPMOST`；`options.modal` ⇒ 建窗后禁用 owner（`EnableWindow(FALSE)`），
    /// [`Self::destroy`] 恢复。owner 的 `WindowId → HWND` 解析在平台事件循环侧完成
    /// （`native_window::resolve_owner`，找不到时诊断降级为 None）。
    ///
    /// 失败一律 `EngineError::WindowCreate`（`CreateWindowExW` / `RegisterClassW` 的 HRESULT 与
    /// `GetLastError` 都写进 `detail`）。
    pub(crate) fn create(
        id: WindowId,
        options: &WindowOptions,
        owner: Option<HWND>,
        dispatcher: Arc<Dispatcher>,
        app: Option<&Rc<RefCell<App>>>,
    ) -> Result<Self, EngineError> {
        // SAFETY: 取本进程模块句柄（`None` = 当前可执行文件）；只读，不转移所有权。
        let instance = unsafe { GetModuleHandleW(None) }
            .map_err(|error| window_error("GetModuleHandleW", error))?;
        register_class(instance)?;

        let class = wide(CLASS_NAME);
        let title = wide(&options.title);
        // 建窗的**临时**几何：客户区尺寸按请求的 scale 估算（真实 DPI 要窗口存在后才能问），
        // 位置按 `position` 换算到目标附近并 clamp 到最近显示器工作区 —— 窗口创建后
        // [`Self::apply_dpi_and_size`] 用**所在显示器 DPI** 重算最终尺寸与落点。
        let (window_x, window_y, provisional_width, provisional_height) =
            provisional_window_rect(options, dpi::system_dpi());
        // SAFETY: `class` / `title` 是本函数栈上的 NUL 结尾 UTF-16 缓冲，`CreateWindowExW`
        // 同步拷贝；实例句柄来自本进程。窗口此时**隐藏**（无 `WS_VISIBLE`）。
        //
        // `WS_EX_NOREDIRECTIONBITMAP`：客户区不做系统重定向表面，全部像素来自 DirectComposition
        // （没有它，DComp 内容会和 GDI 重定向表面叠在一起，resize 时还会多一层 DWM 拉伸）。
        // X1：`WS_EX_TOPMOST` 在创建时声明（置顶窗口），`hWndParent = owner` 走 owner 语义
        // （`WS_OVERLAPPEDWINDOW` 顶层窗口 + owner：owned 恒在 owner 之上、随其最小化/销毁）。
        let ex_style = if options.always_on_top {
            WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOPMOST
        } else {
            WS_EX_NOREDIRECTIONBITMAP
        };
        let hwnd = unsafe {
            CreateWindowExW(
                ex_style,
                windows::core::PCWSTR(class.as_ptr()),
                windows::core::PCWSTR(title.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                window_x,
                window_y,
                provisional_width,
                provisional_height,
                owner,
                None,
                Some(instance.into()),
                None,
            )
        }
        .map_err(|error| window_error("CreateWindowExW", error))?;

        // B2：标题栏明暗跟随系统主题 —— 必须在**首次显示之前**设置，否则深色主题下第一帧
        // 会闪一条亮色标题栏。DWM 属性在隐藏窗口上设置是标准做法；失败时窗口仍可用
        //（`set_titlebar` 内部只记诊断，标题栏退回系统默认亮色）。
        dark_mode::set_titlebar(hwnd, dark_mode::current().unwrap_or(false));

        // 合成目标在窗口显示之前建立：`WS_EX_NOREDIRECTIONBITMAP` 的客户区只有 DComp 内容。
        let composition = match Composition::new(hwnd) {
            Ok(composition) => Arc::new(Mutex::new(composition)),
            Err(error) => {
                // SAFETY: 窗口刚由本函数创建，尚未交给任何其它所有者。
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return Err(error);
            }
        };
        // 系统 size 模态循环里的成帧驱动：有 `App` 才挂（平台单测可以只验证 `WndProc` 记账）。
        let driver = app.map(|app| {
            Rc::new(ResizeFrameDriver::new(
                Rc::downgrade(app),
                id,
                Arc::clone(&composition),
            ))
        });
        let limits = placement::ClientLimits::from_options(options);
        let state = match WndProcState::new(dispatcher, hwnd, id, limits, driver) {
            Ok(state) => Box::into_raw(Box::new(state)),
            Err(error) => {
                // a11y adapter 构造失败（`A11yInit`）：窗口还没显示，直接销毁，避免泄漏 HWND。
                // SAFETY: 窗口刚由本函数创建，尚未交给任何其它所有者（`composition` 随本函数
                // 返回一起释放；DComp 目标引用 HWND，必须先销毁窗口）。
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return Err(error);
            }
        };
        // X1：模态窗口 —— 建窗后（显示前）禁用 owner，系统级拦截 owner 及其非客户区输入
        // （用户无法与 owner 交互，视觉上灰显），关闭/销毁本窗口时由 `destroy` 恢复。
        // 失败（owner 已失效等）只降级为「没有原生模态」：引擎层仍有按 owner 关系过滤输入的
        // 兜底；不 panic、不阻止新窗口创建。放在所有可失败步骤**之后**，避免失败路径把
        // owner 留在禁用状态。
        let modal_owner = if options.modal { owner } else { None };
        if let Some(owner_hwnd) = modal_owner {
            // SAFETY: owner 由调用方保证仍是本进程存活窗口（解析来源是活着的 NativeWindow）；
            // 即使已失效，`EnableWindow` 也只返回 FALSE，不解引用。
            unsafe {
                let _ = EnableWindow(owner_hwnd, false);
            }
        }

        // SAFETY: `hwnd` 刚创建且本函数独占；`state` 的所有权转移给 `NativeWindow`（Drop 回收）。
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize) };

        let mut window = Self {
            id,
            hwnd,
            composition,
            state,
            composition_geometry: None,
            scale: Scale::ONE,
            logical_size: options.size,
            ime_area: None,
            modal_owner,
        };
        window.apply_dpi_and_size(options)?;
        Ok(window)
    }

    /// 合成目标（`render/windows` 在 GPU 线程启动之前挂上组合交换链）。
    ///
    /// 返回 `Arc` 的克隆：调用方（`Renderer::new_for_hwnd`）需要把它带进 GPU 线程共享。
    pub(crate) fn composition(&self) -> Arc<Mutex<Composition>> {
        Arc::clone(&self.composition)
    }

    /// 句柄的 `isize` 形式（引擎内部唯一允许的「无类型」传递）。
    pub(crate) fn hwnd_raw(&self) -> isize {
        self.hwnd.0 as isize
    }

    /// 原生窗口句柄（X1 owner 解析在平台事件循环侧用它建立 `WindowId → HWND` 映射）。
    pub(crate) fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// 创建者传入的窗口 id。
    pub(crate) fn id(&self) -> WindowId {
        self.id
    }

    /// 窗口**当前所在 output** 的刷新率（Hz；A5 的帧预算提示，A3 per-output 收尾）。
    ///
    /// 查询链：`MonitorFromWindow(MONITOR_DEFAULTTONEAREST)` → `GetMonitorInfoW` 取设备名 →
    /// `EnumDisplaySettingsW(ENUM_CURRENT_SETTINGS)` 的 `dmDisplayFrequency`。这是窗口所在显示器
    /// 的**当前模式**，不是桌面级主屏 —— MS 文档明确 DXGI swapchain 只用主屏 cadence
    /// （`render/windows/swapchain.rs` 的 waitable 因此是主屏相位），而 UI 侧的预算门禁用
    /// **本窗口**所在 output 的周期才符合「每 refresh 最多一帧」。
    ///
    /// 任一系统调用失败 ⇒ `None`（平台如实不提供；`Window` 侧据此禁用门禁，退回唤醒即出帧）。
    /// 只在窗口创建 / 显示恢复 / DPI 变化时调用：稳态泵不做系统调用（R07-11 纪律）。
    pub(crate) fn refresh_hz(&self) -> Option<f32> {
        // SAFETY: `self.hwnd` 是仍然存活的本进程窗口（`NativeWindow` 独占）；`MONITORINFOEXW`
        // 的 `cbSize` 按结构体大小填写（`GetMonitorInfoW` 的必需输入）；设备名由系统写入
        // `szDevice`（NUL 结尾），只读传给 `EnumDisplaySettingsW`。
        let monitor = unsafe { MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST) };
        let mut info = MONITORINFOEXW {
            monitorInfo: MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        if !unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) }.as_bool() {
            return None;
        }
        let mut devmode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        // SAFETY: `info.szDevice` 由 `GetMonitorInfoW` 写成 NUL 结尾的设备名；`devmode` 已填
        // `dmSize`（`EnumDisplaySettingsW` 的必需输入），只写本函数栈上的结构体。
        if !unsafe {
            EnumDisplaySettingsW(
                windows::core::PCWSTR(info.szDevice.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut devmode,
            )
        }
        .as_bool()
        {
            return None;
        }
        refresh_hz_from_display_frequency(devmode.dmDisplayFrequency)
    }

    /// 当前 DPI 比例。
    pub(crate) fn scale(&self) -> Scale {
        self.scale
    }

    /// 当前客户区逻辑尺寸。
    pub(crate) fn logical_size(&self) -> Size {
        self.logical_size
    }

    /// 最近一次成功应用到合成树的 `(presented, client)`（泵的 ②b′ 缓存；`None` = 尚未应用）。
    pub(crate) fn composition_geometry(&self) -> Option<((u32, u32), (u32, u32))> {
        self.composition_geometry
    }

    /// 记录一次成功的合成几何应用。
    pub(crate) fn note_composition_geometry(&mut self, geometry: ((u32, u32), (u32, u32))) {
        self.composition_geometry = Some(geometry);
    }

    /// 客户区物理尺寸（创建渲染器 / resize 的唯一输入）。
    pub(crate) fn client_size(&self) -> (u32, u32) {
        let mut rect = RECT::default();
        // SAFETY: 窗口存活期内查询；`rect` 是栈上值。
        let ok = unsafe { GetClientRect(self.hwnd, &mut rect) };
        match ok {
            Ok(()) => (
                (rect.right - rect.left).max(1) as u32,
                (rect.bottom - rect.top).max(1) as u32,
            ),
            Err(_) => physical_client(self.logical_size, self.scale),
        }
    }

    /// 窗口是否最小化。
    ///
    /// 最小化时 `GetClientRect` 是 0×0（[`NativeWindow::client_size`] 会把它钳成 1×1），
    /// 不能拿它当新的渲染目标。
    pub(crate) fn is_minimized(&self) -> bool {
        // SAFETY: 窗口存活期内查询；`IsIconic` 只读窗口状态，不转移所有权。
        unsafe { IsIconic(self.hwnd) }.as_bool()
    }

    /// 显示窗口（`ShowWindow(SW_SHOWNORMAL)`）。
    pub(crate) fn show(&self) {
        // SAFETY: 窗口存活；返回值表示「之前是否可见」，这里不需要。
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNORMAL);
        }
    }

    /// 应用 DPI / 尺寸 / 初始位置：先问窗口**所在显示器**的 DPI，再按
    /// `AdjustWindowRectExForDpi` 调窗口（clamp 后落位），最后回读客户区。
    ///
    /// 落点语义（公共契约：[`WindowOptions::position`]）= **客户区左上角**的逻辑坐标；
    /// 换算顺序是「逻辑点 → 物理原点（round）→ 窗口原点（加非客户区）→ clamp 到工作区」。
    ///
    /// 最多重算两次：建窗的临时落点用主屏 DPI 估算，`SetWindowPos` 跨屏（per-monitor v2）时
    /// 系统可能立即发 `WM_DPICHANGED` 并按建议矩形重排 —— 第二次按**新显示器**的 DPI 重算
    /// 尺寸与落点，收敛到公共契约的「按窗口最终落点显示器的 scale 解释」。
    fn apply_dpi_and_size(&mut self, options: &WindowOptions) -> Result<(), EngineError> {
        let requested = options.size;
        // 0×0 的请求尺寸没有可用的窗口矩形（`physical_client` 只会钳成 1 px）：只记账 DPI，
        // 等平台 resize / `WM_DPICHANGED` 给出真实尺寸。
        if requested.width <= 0.0 || requested.height <= 0.0 {
            let dpi = dpi::window_dpi(self.hwnd);
            let scale = dpi::scale_from_dpi(dpi).ok_or(EngineError::InvalidDpiScale {
                factor: dpi as f32 / dpi::BASE_DPI as f32,
            })?;
            self.scale = scale;
            // SAFETY: `state` 由本窗口独占（`Drop` 之前一直有效）；WndProc 只在消息处理期读它。
            unsafe {
                (*self.state).note_dpi(dpi, scale);
            }
            self.logical_size = requested;
            return Ok(());
        }
        // 应用配置的最小/最大客户区约束在建窗时生效（`WM_GETMINMAXINFO` 的同一个来源，
        // B3）：请求尺寸先钳进 `[min, max]`，避免初始窗口违反自己的约束。
        let size = self.state_mut().client_limits().clamp(requested);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let dpi = dpi::window_dpi(self.hwnd);
            let scale = dpi::scale_from_dpi(dpi).ok_or(EngineError::InvalidDpiScale {
                factor: dpi as f32 / dpi::BASE_DPI as f32,
            })?;
            self.scale = scale;
            // `WM_GETMINMAXINFO` 的 `ptMinTrackSize` / `ptMaxTrackSize` 与 `WM_DPICHANGED`
            // 用**同一个**换算（[`placement::ClientLimits::track_sizes`]）：`WndProcState` 是
            // 唯一持有者，两处分别算会在 DPI 变化后漂移（P2-1：直接透传客户区尺寸会小一个
            // 边框 + 标题栏）。
            // SAFETY: `state` 由本窗口独占；WndProc 只在消息处理期读它。
            unsafe {
                (*self.state).note_dpi(dpi, scale);
            }
            let client = physical_client(size, scale);
            let (width, height) = dpi::window_size_for_client(client, dpi);
            match options.position {
                Some(position) => {
                    let client_origin = dpi::physical_origin(position, scale);
                    let (x, y) = dpi::window_origin_for_client(client_origin, client, dpi);
                    // 公共契约：位置 clamp 到最近显示器工作区（至少标题栏可见）。
                    let target =
                        PhysicalRect::new(x, y, x.saturating_add(width), y.saturating_add(height));
                    let placed = placement::clamp_to_work_area(target, &placement::work_areas());
                    // SAFETY: 窗口存活；只改尺寸与位置，不重新创建、不激活窗口、不改 Z 序。
                    // 可能同步重入 `WndProc`（`WM_WINDOWPOSCHANGED` / `WM_SIZE` /
                    // `WM_DPICHANGED`）：此处不持有 `&mut WndProcState`（`note_dpi` 的借用已结束）。
                    unsafe {
                        SetWindowPos(
                            self.hwnd,
                            None,
                            placed.x0,
                            placed.y0,
                            placed.width(),
                            placed.height(),
                            SWP_NOZORDER | SWP_NOACTIVATE,
                        )
                    }
                    .map_err(|error| window_error("SetWindowPos", error))?;
                }
                None => {
                    // 保持建窗时的位置（[`DEFAULT_WINDOW_ORIGIN`] 或临时落点），只改尺寸。
                    //
                    // SAFETY: 同上。
                    unsafe {
                        SetWindowPos(
                            self.hwnd,
                            None,
                            0,
                            0,
                            width,
                            height,
                            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE,
                        )
                    }
                    .map_err(|error| window_error("SetWindowPos", error))?;
                }
            }
            // 跨屏落位触发了 `WM_DPICHANGED`（系统会先按建议矩形重排）：按新显示器 DPI
            // 再算一次。第二次之后不再重试（DPI 只可能随落点变化一次；防御性上限）。
            if attempt >= 2 || dpi::window_dpi(self.hwnd) == dpi {
                break;
            }
        }
        let scale = self.scale;
        // 回读：客户区由系统按真实边框算出，逻辑尺寸以它为准（round 之后可能与请求差 1 px）。
        let client = self.client_size();
        self.note_client_resize(scale, client);
        Ok(())
    }

    /// `WndProc` 状态的独占访问（事件循环消费事件 / 读修饰键）。
    ///
    /// **调用方纪律**（R07-7）：拿到 `&mut WndProcState` 之后不得调用任何会同步重入
    /// `WndProc` 的 Win32 API（`SetWindowPos` / `ShowWindow` / `ImmAssociateContext*` /
    /// `ImmSet*Window` / 会进模态循环的 `DefWindowProcW`）。这些调用一律走
    /// 「短借用算命令 → 释放借用执行 → 短借用写回」的两阶段（见 [`Self::sync_ime_enabled`]）。
    pub(crate) fn state_mut(&mut self) -> &mut WndProcState {
        // SAFETY: `state` 由 `create` 的 `Box::into_raw` 产生、`Drop` 回收；窗口存活期内唯一所有者。
        unsafe { &mut *self.state }
    }

    /// 交付一笔 a11y 更新（`pump` 每轮调用；`a11y_pending` 为 O(1) 过滤）。
    ///
    /// `build_update` 只在 `update_if_active` 的闭包里执行 ⇒ 没有辅助技术时**没有任何**
    /// `accesskit::Node` 构造（懒激活零开销）。
    pub(crate) fn update_a11y(&mut self, app: &mut crate::app::App) {
        let window = self.id;
        if !app.a11y_pending(window) {
            return;
        }
        // 短借用：`QueuedEvents::raise` 会**同步**派发嵌套 `WM_GETOBJECT`，所以只能在不持有
        // `&mut WndProcState` 时调用（同 P11 的 `WM_GETOBJECT` / `focus_changed` 纪律）。
        // 丢弃 `Option<QueuedEvents>` 等于只交付首棵树、后续事件全丢（P14 审查 #1）。
        let events = {
            let state = self.state_mut();
            state
                .a11y_mut()
                .update(|| crate::a11y::adapter::build_update(app, window))
        };
        if let Some(events) = events {
            events.raise();
        }
    }

    /// 同步 IME caret 区域到 IMM32（每轮泵调用，独立于帧门禁；缓存键含 Scale，DPI 变化会重发）。
    ///
    /// **两阶段**（R07-7）：命令在短借用里算出来，执行时**不持有 `&mut WndProcState`** ——
    /// `ImmSetCompositionWindow` / `ImmSetCandidateWindow` 会同步派发
    /// `WM_IME_NOTIFY(IMN_SETCOMPOSITIONWINDOW)`，嵌套进入的 `WndProc` 会再构造
    /// `&mut WndProcState`，同时持有两个 `&mut` 是别名 UB。
    pub(crate) fn sync_ime_cursor_area(&mut self, app: &App) {
        let area = match app.ime_cursor_area(self.id) {
            Ok(area) => area,
            // 文本尚未 shape / 文本刚变化：保留上一次有效区域，等下一轮。
            Err(_) => return,
        };
        let next = area.map(|rect| (rect, self.scale));
        if self.ime_area == next {
            return;
        }
        self.ime_area = next;
        let hwnd = self.hwnd;
        let physical = area.map(|rect| rect.to_physical(self.scale));
        let command = self.state_mut().plan_ime_cursor_area(physical);
        if let Some(command) = command {
            let applied = ime::execute(hwnd, command);
            self.state_mut().note_ime_applied(applied);
        }
    }

    /// 同步 IME 启用状态到 IMM32（每轮泵调用；目标状态来自 `App::ime_enabled`）。
    ///
    /// 禁用时把输入上下文从窗口上摘下来（`ImmAssociateContext(hwnd, NULL)`），启用时还回去
    /// （见 `platform/windows/ime.rs`）。状态切换会清空 `ime_area` 缓存：恢复的上下文可能是
    /// 系统新建的默认上下文，之前写的组合/候选窗表单不保证还在，下一轮
    /// [`Self::sync_ime_cursor_area`] 必须重发。
    ///
    /// 两阶段执行的原因同 [`Self::sync_ime_cursor_area`]：`ImmAssociateContext*` 会同步派发
    /// `WM_IME_SETCONTEXT`。
    pub(crate) fn sync_ime_enabled(&mut self, app: &App) {
        let enabled = app.ime_enabled(self.id);
        if self.state_mut().ime_enabled() == enabled {
            return;
        }
        let hwnd = self.hwnd;
        let command = self.state_mut().plan_ime_enabled(enabled);
        if let Some(command) = command {
            let applied = ime::execute(hwnd, command);
            self.state_mut().note_ime_applied(applied);
        }
        self.ime_area = None;
    }

    /// 把 `App` 里最新的自绘标题栏拖动区对账到 `WndProcState`（X2；泵每轮调用）。
    ///
    /// **设计**：`WndProc` 不调用应用层（平台层纪律，也避免消息处理期间借用 `App`），区域由
    /// 泵每轮从 `App::window(id).drag_regions()` 拷进 per-window 状态；`Window::set_drag_regions`
    /// 同值 no-op，这里再比一次避免每轮分配（同值时不碰 `Vec`）。
    /// 区域是 `Window::set_drag_regions` 的**逻辑客户区坐标**，`WM_NCHITTEST` 在命中时返回
    /// `HTCAPTION`（走系统 move 模态循环）。
    pub(crate) fn sync_drag_regions(&mut self, app: &App) {
        let Some(window) = app.window(self.id) else {
            return;
        };
        self.state_mut().set_drag_regions(window.drag_regions());
    }

    /// 窗口是否已被销毁（`close_requested_windows` 的 retain 判据）。
    pub(crate) fn is_destroyed(&self) -> bool {
        self.hwnd.is_invalid()
    }

    /// 只记账新 DPI / scale（**几何已经由 `WndProc` 在 `WM_DPICHANGED` 里改完**，R07-12）。
    ///
    /// 同时清掉 caret hint：旧的 `cursor_area` 是旧 DPI 下的物理像素，DPI 变化后在下一次
    /// `sync_ime_cursor_area` 重新下发前不得被 `WM_IME_STARTCOMPOSITION` 复用。
    pub(crate) fn note_scale(&mut self, scale: Scale, dpi: u32) {
        self.scale = scale;
        self.state_mut().note_dpi(dpi, scale);
        // `None` 命令 = 只清 hint（没有要执行的 IMM32 调用）。
        let _ = self.state_mut().plan_ime_cursor_area(None);
        self.ime_area = None;
    }

    /// 同步 `logical_size` 字段（客户区被用户拖动改变之后由事件循环调用）。
    ///
    /// `scale` 必须是**该 `WM_SIZE` 消息产生时**的比例（R07-12）：`WM_SIZE` 与
    /// `WM_DPICHANGED` 可能同批到达，用窗口侧的旧 scale 会把逻辑尺寸算错。
    pub(crate) fn note_client_resize(&mut self, scale: Scale, client: (u32, u32)) {
        self.logical_size = Size::new(
            scale.to_logical(client.0 as f32),
            scale.to_logical(client.1 as f32),
        );
    }

    /// 重新查询显示器信息并写回 `App` 窗口（R07-14）。
    ///
    /// 消费者：`native_window::apply_display_recovery`（显示器拓扑变化 / 工作区变化 / 睡眠
    /// 唤醒）。引擎持有的显示派生状态只有两项：**本窗口所在显示器的 DPI**
    /// （`GetDpiForWindow`）与**客户区几何**（`GetClientRect`）—— 两者是布局与渲染目标的输入，
    /// 睡眠唤醒或拔插显示器之后它们可能已经被系统改掉，而 `WM_DPICHANGED` / `WM_SIZE` 不保证
    /// 补发（同尺寸换屏、窗口被系统搬走、唤醒后系统只发一条 `WM_DISPLAYCHANGE`）。
    ///
    /// 写回顺序与 `WM_DPICHANGED` 的路径一致（`native_window::apply_event`）：先 `set_scale`
    /// （布局 + 全量 damage + 文本重新 shape），再 `set_size` / `set_surface`（渲染目标）。
    /// 最小化时客户区是 0×0，只更新比例，目标保持上一次真实值（由恢复后的 `WM_SIZE` 覆盖）。
    pub(crate) fn requery_display(&mut self, app: &mut App) {
        let dpi = dpi::window_dpi(self.hwnd);
        // 非法 DPI（0 / 非有限）：保留旧值，等下一次 `WM_DPICHANGED` 纠正。
        let Some(scale) = dpi::scale_from_dpi(dpi) else {
            return;
        };
        if scale != self.scale {
            // 同步 `WndProcState` 的 DPI（`WM_GETMINMAXINFO` / 同批事件换算读它），并清掉旧
            // DPI 下的 IME caret hint（恢复后重发）。
            self.note_scale(scale, dpi);
        }
        let Some(window) = app.window_mut(self.id) else {
            return;
        };
        // A5：显示 / 电源恢复后刷新率提示也要重查（换屏 / 改刷新率 / 唤醒后模式重置）。
        if let Some(hz) = self.refresh_hz() {
            window.set_frame_refresh_hz(hz);
        }
        // 比例以**平台真值 vs `Window` 现值**比较：拓扑变化不一定改 DPI，而 `set_scale`
        // 会整树重排 + 全部文本重新 shape，不该白付这份代价（同值不写）。
        if window.scale() != scale {
            window.set_scale(scale);
        }
        if self.is_minimized() {
            return;
        }
        let client = self.client_size();
        self.note_client_resize(scale, client);
        window.set_size(Size::new(
            scale.to_logical(client.0 as f32),
            scale.to_logical(client.1 as f32),
        ));
        window.set_surface(client);
    }

    /// 销毁窗口（幂等；`Drop` 也会调）。
    pub(crate) fn destroy(&mut self) {
        // X1：模态窗口销毁前先恢复 owner 的输入。顺序不能反：owned 窗口先没会让 owner 永久
        // 禁用（灰显 + 收不到任何输入）。owner 已被销毁（系统随 owner 销毁 owned 窗口）时
        // `EnableWindow` 对失效句柄只返回 FALSE，不 UB。
        if let Some(owner) = self.modal_owner.take() {
            // SAFETY: owner 句柄来自本进程活着的 `NativeWindow`（解析时保证）；失效时调用只返回 FALSE。
            unsafe {
                let _ = EnableWindow(owner, true);
            }
        }
        if self.hwnd.is_invalid() {
            return;
        }
        // 应用禁用 IME 时输入上下文被摘下来存在 `WndProcState` 里：窗口销毁时系统只释放
        // **仍关联**在窗口上的上下文，必须先还回去，否则是 `HIMC` 泄漏（P2-7）。
        // 两阶段：`ImmAssociateContext` 会同步派发 `WM_IME_SETCONTEXT`，执行时不能持有 `&mut`。
        let hwnd = self.hwnd;
        let command = self.state_mut().plan_ime_release();
        if let Some(command) = command {
            let applied = ime::execute(hwnd, command);
            self.state_mut().note_ime_applied(applied);
        }
        // 顺序：合成目标引用 HWND，必须**在 HWND 仍然存活时**把 visual 从
        // 窗口上摘下来并释放全部 DComp COM 对象（`SetRoot(NULL)` + `Commit`，再逐个 drop）。
        // 关闭顺序保证 GPU 线程已经退出（`pump` 先 drop `App`，`Renderer::drop` join 线程），
        // 因此这里能拿到互斥锁且不会再有人碰合成树。
        composition::lock(&self.composition).shutdown();
        // SAFETY: 窗口存活；销毁后把句柄置空避免二次销毁。
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
        self.hwnd = HWND::default();
    }
}

impl Drop for NativeWindow {
    fn drop(&mut self) {
        self.destroy();
        if !self.state.is_null() {
            // SAFETY: 指针由 `create` 里的 `Box::into_raw` 产生，只在这里回收一次。
            unsafe {
                drop(Box::from_raw(self.state));
            }
            self.state = std::ptr::null_mut();
        }
    }
}

/// 建窗前的**临时**窗口外框 `(x, y, width, height)`（物理像素）。
///
/// 窗口还不存在、问不到 `GetDpiForWindow`：用主屏 DPI 粗定位 —— `position` 是逻辑客户区
/// 左上角，换算到物理后 clamp 到最近显示器工作区，让隐藏窗口**先建在目标附近**；
/// [`NativeWindow::apply_dpi_and_size`] 随后按窗口所在显示器 DPI 重算最终尺寸与落点。
/// `position = None` ⇒ 确定性的 [`DEFAULT_WINDOW_ORIGIN`]。
fn provisional_window_rect(options: &WindowOptions, dpi: u32) -> (i32, i32, i32, i32) {
    let client = physical_client(options.size, options.scale);
    let (width, height) = dpi::window_size_for_client(client, dpi);
    let Some(position) = options.position else {
        return (
            DEFAULT_WINDOW_ORIGIN.0,
            DEFAULT_WINDOW_ORIGIN.1,
            width,
            height,
        );
    };
    let Some(scale) = dpi::scale_from_dpi(dpi) else {
        return (
            DEFAULT_WINDOW_ORIGIN.0,
            DEFAULT_WINDOW_ORIGIN.1,
            width,
            height,
        );
    };
    let origin = dpi::window_origin_for_client(dpi::physical_origin(position, scale), client, dpi);
    let target = PhysicalRect::new(
        origin.0,
        origin.1,
        origin.0.saturating_add(width),
        origin.1.saturating_add(height),
    );
    let placed = placement::clamp_to_work_area(target, &placement::work_areas());
    (placed.x0, placed.y0, placed.width(), placed.height())
}

/// 逻辑尺寸 × scale → 物理客户区（足够大，至少 1 px）。
fn physical_client(size: Size, scale: Scale) -> (u32, u32) {
    let rect = Rect::new(0.0, 0.0, size.width, size.height).to_physical_covering(scale);
    (rect.width().max(1) as u32, rect.height().max(1) as u32)
}

/// 注册窗口类；`ERROR_CLASS_ALREADY_EXISTS`（1410）视为成功。
fn register_class(instance: windows::Win32::Foundation::HMODULE) -> Result<(), EngineError> {
    let class = wide(CLASS_NAME);
    let wc = WNDCLASSW {
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance.into(),
        hIcon: HICON::default(),
        // 类光标必须是箭头：probe 实测「类光标 = null + HTCLIENT」会得到**空光标**（不可见），
        // 而 `DefWindowProcW` 会用类光标兜底客户区与非客户区（P10 F-96）。
        hCursor: load_arrow_cursor(),
        // 引擎自己画每一帧：不擦背景，避免 `WM_ERASEBKGND` 闪白。
        hbrBackground: HBRUSH::default(),
        lpszMenuName: windows::core::PCWSTR::null(),
        lpszClassName: windows::core::PCWSTR(class.as_ptr()),
    };
    // SAFETY: `wc` 的字符串指针指向本函数栈上的 `class`，`RegisterClassW` 同步拷贝。
    let atom = unsafe { RegisterClassW(&wc) };
    if atom != 0 {
        return Ok(());
    }
    let error = unsafe { windows::Win32::Foundation::GetLastError() }.0;
    if error == 1410 {
        return Ok(());
    }
    Err(EngineError::WindowCreate {
        detail: smol_str::format_smolstr!("RegisterClassW: GetLastError={error}"),
    })
}

fn window_error(stage: &'static str, error: windows::core::Error) -> EngineError {
    EngineError::WindowCreate {
        detail: smol_str::format_smolstr!("{stage}: {:#010X}", error.code().0 as u32),
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use windows::Win32::UI::{
        Input::{
            Ime::{ImmGetContext, ImmReleaseContext},
            KeyboardAndMouse::IsWindowEnabled,
        },
        WindowsAndMessaging::{
            GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetWindow, GetWindowLongPtrW, IsWindow, WS_CHILD,
            WS_EX_TOPMOST,
        },
    };

    use super::{NativeWindow, refresh_hz_from_display_frequency};
    use crate::{
        app::App,
        math::{Rect, Scale, Size},
        platform::dispatcher::Dispatcher,
        window::{WindowId, WindowOptions},
    };
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    /// win-1 P3（任务 2）：`NativeWindow::sync_ime_enabled` 必须把 `App` 的目标状态应用到
    /// IMM32；`NativeWindow::destroy` 必须在 `DestroyWindow` 之前把摘下的上下文还回去。
    ///
    /// 这条用例覆盖的是 `NativeWindow` 的**调用点**：删掉 `sync_ime_enabled` 里的目标状态
    /// 比较 / `ime_area` 清理，或删掉 `destroy` 里的 `plan_ime_release` 执行时，
    /// `ImeState` 自己的状态机测试仍会全绿，这里会失败。泵**每轮调用**它们的节奏不在单测
    /// 范围内（需要真实泵 + 渲染器），属于真机验证项。
    #[test]
    fn sync_ime_enabled_applies_the_app_target_state_and_destroy_restores_the_context() {
        let runtime = runtime();
        let mut app = App::new(runtime.handle().clone());
        let options = WindowOptions::default();
        let id = app.create_window(options.clone());
        let mut native = NativeWindow::create(id, &options, None, Dispatcher::new(), None)
            .expect("NativeWindow::create");

        // SAFETY: 窗口由本用例持有且存活；上下文句柄成对释放。
        let baseline = unsafe { ImmGetContext(native.hwnd) };
        if !baseline.is_invalid() {
            // SAFETY: 同上。
            unsafe {
                let _ = ImmReleaseContext(native.hwnd, baseline);
            }
        }

        // 目标状态与已应用状态相同：`sync_ime_enabled` 直接返回，不清 caret 缓存。
        native.ime_area = Some((Rect::new(0.0, 0.0, 1.0, 18.0), Scale::ONE));
        native.sync_ime_enabled(&app);
        assert!(
            native.ime_area.is_some(),
            "目标状态未变化时不得清 caret 缓存"
        );

        // 禁用：`App` 目标状态变化 ⇒ 摘除上下文，并清 caret 缓存（恢复的上下文不保证保留
        // 之前写的组合/候选窗表单，下一轮必须重发）。
        app.set_ime_enabled(id, false);
        native.sync_ime_enabled(&app);
        assert!(
            !native.state_mut().ime_enabled(),
            "禁用态必须应用到平台状态"
        );
        assert!(native.ime_area.is_none(), "状态切换必须清 caret 缓存");
        // SAFETY: 窗口仍存活。
        assert!(
            unsafe { ImmGetContext(native.hwnd) }.is_invalid(),
            "禁用后窗口不得再关联输入上下文"
        );

        // 重新启用：还回保存的上下文（没有保存值则 `IACE_DEFAULT` 让系统关联默认上下文）。
        app.set_ime_enabled(id, true);
        native.sync_ime_enabled(&app);
        assert!(native.state_mut().ime_enabled(), "启用态必须应用到平台状态");
        // SAFETY: 同上。
        let restored = unsafe { ImmGetContext(native.hwnd) };
        assert!(!restored.is_invalid(), "重新启用必须恢复输入上下文");
        if !baseline.is_invalid() {
            assert_eq!(restored, baseline, "恢复的必须是被摘下的那个上下文");
        }
        // SAFETY: 同上。
        unsafe {
            let _ = ImmReleaseContext(native.hwnd, restored);
        }

        // 销毁前恢复：`destroy` 必须先执行 `plan_ime_release` 的命令（它会把已应用状态置回
        // 启用）；若这一步被删掉，状态会停在禁用态。
        app.set_ime_enabled(id, false);
        native.sync_ime_enabled(&app);
        assert!(!native.state_mut().ime_enabled(), "前置：窗口处于禁用态");
        native.destroy();
        assert!(
            native.state_mut().ime_enabled(),
            "destroy 必须先恢复 IME 关联状态，否则 HIMC 泄漏"
        );
    }

    /// R07-14：恢复路径必须把**当前**显示器信息（DPI + 客户区）写回 `App` 窗口 —— 这是
    /// 「重新查询显示器」这一步的全部意义：睡眠唤醒 / 拓扑变化之后 `Window` 里的旧值不可信。
    #[test]
    fn requery_display_writes_the_live_scale_and_client_size_back() {
        let runtime = runtime();
        let mut app = App::new(runtime.handle().clone());
        let options = WindowOptions::default();
        let id = app.create_window(options.clone());
        let mut native = NativeWindow::create(id, &options, None, Dispatcher::new(), None)
            .expect("NativeWindow::create");

        // 故意把 App 侧写成与平台不一致的值：恢复必须把它纠回平台真值。
        {
            let window = app.window_mut(id).expect("window");
            window.set_scale(Scale::new(3.0).expect("scale"));
            window.set_size(Size::new(1.0, 1.0));
        }
        native.requery_display(&mut app);

        let window = app.window(id).expect("window");
        assert_eq!(
            window.scale(),
            native.scale(),
            "恢复必须重新查询显示器 DPI 并写回比例"
        );
        assert_eq!(
            window.size(),
            native.logical_size(),
            "恢复必须重新查询客户区并写回逻辑尺寸"
        );
    }

    /// A5：刷新率提示的解析 —— `dmDisplayFrequency` 的 0 / 1 是「硬件默认 / 驱动决定」，
    /// 不是可用周期，必须按未知处理（否则会把 1 Hz 当成刷新率把帧卡死）。
    #[test]
    fn a_display_mode_frequency_becomes_a_refresh_hint() {
        assert_eq!(refresh_hz_from_display_frequency(60), Some(60.0));
        assert_eq!(refresh_hz_from_display_frequency(144), Some(144.0));
        assert_eq!(
            refresh_hz_from_display_frequency(0),
            None,
            "0 = 硬件默认，不可用"
        );
        assert_eq!(
            refresh_hz_from_display_frequency(1),
            None,
            "1 = 驱动决定，不可用"
        );
    }

    /// A5：真实窗口的刷新率查询必须在 Windows 本机给出**合理值**（24–480 Hz）。
    ///
    /// 环境（远程桌面 / 无显示器会话）可能查不到 ⇒ `None` 也是合法结果；有值时必须是真实
    /// 刷新率量级，不能是 0 / 1 / 垃圾值。变异：把 `refresh_hz_from_display_frequency` 改成恒
    /// `Some(frequency as f32)` 后，本测试在 `dmDisplayFrequency = 0` 的会话上会失败。
    #[test]
    fn a_real_window_reports_a_plausible_refresh_rate() {
        let options = WindowOptions::default();
        let native = NativeWindow::create(
            crate::window::WindowId::for_test(1),
            &options,
            None,
            Dispatcher::new(),
            None,
        )
        .expect("NativeWindow::create");
        match native.refresh_hz() {
            None => {}
            Some(hz) => {
                assert!(
                    (24.0..=480.0).contains(&hz),
                    "刷新率提示必须是真实量级：{hz}"
                );
                assert!(
                    hz.fract() == 0.0,
                    "EnumDisplaySettings 的频率是整数 Hz：{hz}"
                );
            }
        }
    }

    // ── X1：owner / 模态 / 置顶的原生语义（隐藏窗口夹具）────────────────────────────────
    //
    // 这里只钉**平台层**的系统行为（Win32 owner / WS_EX_TOPMOST / EnableWindow）；真机 z 序、
    // 最小化跟随与置顶视觉由 `.review/p1-b-win-x1.log` 的可见窗口探针验收。引擎层的 owner
    // 校验 / 级联 / 模态输入过滤在 App 层（本 worktree 之外），不在本用例范围。

    /// 建一个隐藏的普通顶层窗口（X1 用例的 owner / 对照）。
    fn window(id: u32) -> NativeWindow {
        NativeWindow::create(
            WindowId::for_test(id),
            &WindowOptions::default(),
            None,
            Dispatcher::new(),
            None,
        )
        .expect("NativeWindow::create")
    }

    /// X1 owner：`CreateWindowExW` 的 `hWndParent` 走 owner 语义 ——
    /// `GetWindow(GW_OWNER)` 回指 owner，且窗口**不是** `WS_CHILD`（owned 窗口仍是顶层窗口）。
    ///
    /// 变异守卫：把 `CreateWindowExW` 的 `hWndParent` 改回 `None` ⇒ `GW_OWNER` 为 0，用例红。
    #[test]
    fn an_owned_window_reports_its_owner_and_is_not_a_child_window() {
        let owner = window(1);
        let options = WindowOptions {
            owner: Some(WindowId::for_test(1)),
            ..WindowOptions::default()
        };
        let owned = NativeWindow::create(
            WindowId::for_test(2),
            &options,
            Some(owner.hwnd),
            Dispatcher::new(),
            None,
        )
        .expect("owned window");

        // SAFETY: 两个窗口都由本用例持有且仍存活；只读查询。
        let reported = unsafe { GetWindow(owned.hwnd, GW_OWNER) }.expect("GetWindow(GW_OWNER)");
        assert_eq!(
            reported, owner.hwnd,
            "owned 窗口的 GW_OWNER 必须回指 owner（hWndParent owner 语义）"
        );
        // SAFETY: 同上，只读查询窗口样式。
        let style = unsafe { GetWindowLongPtrW(owned.hwnd, GWL_STYLE) };
        assert_eq!(
            style & (WS_CHILD.0 as isize),
            0,
            "owner 语义不得退化成客户区子窗口（window_set/origin 行为会完全不同）"
        );
    }

    /// X1 置顶：`always_on_top` ⇒ 创建时 `WS_EX_TOPMOST`；普通窗口不得带该位。
    ///
    /// 变异守卫：删掉 `CreateWindowExW` 的 `WS_EX_TOPMOST` ⇒ 断言红。
    #[test]
    fn an_always_on_top_window_has_the_topmost_ex_style() {
        let options = WindowOptions {
            always_on_top: true,
            ..WindowOptions::default()
        };
        let topmost = NativeWindow::create(
            WindowId::for_test(3),
            &options,
            None,
            Dispatcher::new(),
            None,
        )
        .expect("topmost window");
        // SAFETY: 窗口由本用例持有且仍存活；只读查询扩展样式。
        let ex_style = unsafe { GetWindowLongPtrW(topmost.hwnd, GWL_EXSTYLE) };
        assert_ne!(
            ex_style & (WS_EX_TOPMOST.0 as isize),
            0,
            "always_on_top 窗口必须带 WS_EX_TOPMOST"
        );

        let plain = window(4);
        // SAFETY: 同上。
        let plain_ex_style = unsafe { GetWindowLongPtrW(plain.hwnd, GWL_EXSTYLE) };
        assert_eq!(
            plain_ex_style & (WS_EX_TOPMOST.0 as isize),
            0,
            "普通窗口不得带 WS_EX_TOPMOST"
        );
    }

    /// X1 模态：建窗后系统级禁用 owner（`IsWindowEnabled == false`），模态窗口销毁后恢复。
    ///
    /// 变异守卫：删掉 `create` 里的 `EnableWindow(owner, false)` ⇒ 第一段断言红；
    /// 删掉 `destroy` 里的 `EnableWindow(owner, true)` ⇒ 第二段断言红。
    #[test]
    fn a_modal_window_disables_its_owner_until_it_is_destroyed() {
        let owner = window(5);
        let options = WindowOptions {
            owner: Some(WindowId::for_test(5)),
            modal: true,
            ..WindowOptions::default()
        };
        let mut modal = NativeWindow::create(
            WindowId::for_test(6),
            &options,
            Some(owner.hwnd),
            Dispatcher::new(),
            None,
        )
        .expect("modal window");
        // SAFETY: 两个窗口仍存活；只读查询启用状态。
        assert!(
            !unsafe { IsWindowEnabled(owner.hwnd) }.as_bool(),
            "模态窗口打开期间 owner 必须被系统禁用（EnableWindow(FALSE)）"
        );

        modal.destroy();
        // SAFETY: owner 仍存活；只读查询。
        assert!(
            unsafe { IsWindowEnabled(owner.hwnd) }.as_bool(),
            "模态窗口销毁后必须恢复 owner 输入（EnableWindow(TRUE)）"
        );
    }

    /// X1 模态反例：非模态的 owned 窗口不得禁用 owner。
    #[test]
    fn an_owned_window_without_modal_leaves_its_owner_enabled() {
        let owner = window(7);
        let options = WindowOptions {
            owner: Some(WindowId::for_test(7)),
            ..WindowOptions::default()
        };
        let _owned = NativeWindow::create(
            WindowId::for_test(8),
            &options,
            Some(owner.hwnd),
            Dispatcher::new(),
            None,
        )
        .expect("owned window");
        // SAFETY: 两个窗口仍存活；只读查询。
        assert!(
            unsafe { IsWindowEnabled(owner.hwnd) }.as_bool(),
            "非模态的 owned 窗口不得禁用 owner"
        );
    }

    /// X1 生命周期契约（系统语义，记录用）：owner 被 `DestroyWindow` 时系统**一并销毁**其
    /// owned 窗口（`WindowOptions::owner` 文档承诺的「owner 销毁时 owned 一并销毁」）。
    ///
    /// 引擎正常关闭路径由 App 层先关闭 owned 再关闭 owner（级联在 App 层实现）；本用例验证
    /// 平台层对「句柄被系统偷走」的容错：`IsWindow` 变 false，且本层 `destroy()` 对失效句柄
    /// 安全失败（不 panic / 不 UB）。
    #[test]
    fn destroying_the_owner_takes_the_owned_window_with_it() {
        let owner = window(9);
        let options = WindowOptions {
            owner: Some(WindowId::for_test(9)),
            ..WindowOptions::default()
        };
        let mut owned = NativeWindow::create(
            WindowId::for_test(10),
            &options,
            Some(owner.hwnd),
            Dispatcher::new(),
            None,
        )
        .expect("owned window");
        let owned_hwnd = owned.hwnd;

        drop(owner);
        // SAFETY: 只读查询；句柄预期已随 owner 销毁而失效。
        assert!(
            !unsafe { IsWindow(Some(owned_hwnd)) }.as_bool(),
            "owner 销毁时系统必须一并销毁 owned 窗口（owner 语义契约）"
        );

        // 引擎侧销毁路径对已经失效的句柄必须安全失败：`Composition::shutdown` 忽略错误、
        // `DestroyWindow` 返回错误被忽略，不 panic。
        owned.destroy();
    }
}
