//! 系统 size 模态循环里的**真实帧**驱动（与 Chrome / Qt 同构）。
//!
//! 拖动边框时系统进入自己的 size 模态循环：它把 `WM_SIZE` / `WM_PAINT` **同步 `DispatchMessage`**
//! 到本窗口的 `WndProc`，但不会回到引擎的泵 —— 泵的 ④（`App::frame`）在这整段时间不运行。
//! 产品前提是「不用定时器驱动刷新」+「resize 必须真实重排 + 重绘，不能拉伸旧帧」，所以重排 /
//! 重绘必须由**模态循环分派的消息**直接驱动，而不是由引擎的顶层消息循环驱动。
//!
//! 交互本身仍归系统（`WM_NCLBUTTONDOWN` 的 8 个命中码照常 `DefWindowProcW`）：Aero Snap /
//! Win11 Snap Layouts / 双击边框 / `Alt+Space` → 大小 / 拖动中切换显示器全部免费，且与
//! Chrome / Qt 的实际做法一致（Qt 的 `QWindow::startSystemResize` 在 Windows 上就是
//! `PostMessage(WM_SYSCOMMAND, SC_SIZE…)`，把交互交还系统）。
//!
//! 本模块只做「一条 `WM_SIZE` → 一步帧」的适配，顺序与泵的 ②b′/④ 相同：
//!
//! 1. 客户区物理尺寸 + 当前比例 → 逻辑尺寸 / surface（布局输入先落地）；
//! 2. DirectComposition 几何：内容 = 最近一次**完成 present** 的尺寸（恒 1:1、从不拉伸），
//!    客户区 = 当前；新暴露区域由背景层填充，最多一个 vsync 的接缝；
//! 3. `App::frame`：重排 → 按新 surface 重绘 → 提交 → 发布给合成线程。
//!
//! **重入安全**：泵在 `DispatchMessageW` 期间不持有 `App` 借用（见 `pump_loop` 的 ① 与 ② 分段），
//! 所以模态循环里的这一步能拿到独占借用；万一恰好有别的借用（帧内重入），`try_borrow_mut`
//! 失败就放弃本步（下一条 `WM_SIZE` 会重试），绝不构造第二个 `&mut App`。
//!
//! 失败不在这里 `return`（`WndProc` 没有错误通道）：登记进 [`super::WndProcState`] 的
//! `pending_error`，由泵的下一轮 ② 冒泡（帧管线错误不可重试，绝不能静默）。
//!
//! **范围**：只做「像素 / 布局 / 渲染目标」。a11y 树交付与 IME caret 表单仍由泵的 ④′ 负责
//! （它们需要 adapter / IMM32 上下文，且拖拽期间没有键盘输入）：一次拖动结束后的下一轮泵会
//! 把最终几何同步给 AT / IME，拖拽过程中的中间尺寸不必逐条通知。

use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    sync::{Arc, Mutex},
};

use crate::{
    app::App,
    error::EngineError,
    math::{Scale, Size},
    platform::windows::composition::{self, Composition},
    window::{Window, WindowId},
};

/// 一条待执行的 `WM_SIZE` 帧步骤：在 `&mut WndProcState` 的短借用里取走，**出借用后**执行。
///
/// 两阶段是文件头纪律 1：`App::frame` 的调用面很大（用户布局 / 样式闭包、渲染发布），
/// 把「驱动一帧」放进 `&mut WndProcState` 的借用里等于赌它永远不会重入 `WndProc`；
/// 拆成「短借用取 [`ResizeStep`] → 借用外 `run` → 短借用写回错误」就不再依赖这个假设。
pub(crate) struct ResizeStep {
    driver: Rc<ResizeFrameDriver>,
    scale: Scale,
    client: (u32, u32),
}

impl ResizeStep {
    pub(crate) fn new(driver: Rc<ResizeFrameDriver>, scale: Scale, client: (u32, u32)) -> Self {
        Self {
            driver,
            scale,
            client,
        }
    }

    /// 执行一步（不持有 `WndProcState` 借用）；返回需要冒泡的粘性错误。
    pub(crate) fn run(self) -> Option<EngineError> {
        self.driver.frame(self.scale, self.client)
    }
}

/// 每窗口一个：把系统派发的一条 `WM_SIZE` 变成「更新客户区 + 一帧真实帧」。
pub(crate) struct ResizeFrameDriver {
    /// 泵持有的 `App`（弱引用：驱动随窗口存活，但**不得**延长 `App` 的生命周期 ——
    /// 关闭顺序要求 `App`（及其 `Renderer` 的 GPU 线程 join）先于原生窗口销毁）。
    app: Weak<RefCell<App>>,
    window: WindowId,
    /// 与 `NativeWindow` 共享的合成目标（`Arc<Mutex<Composition>>`，与 GPU 线程串行化）。
    composition: Arc<Mutex<Composition>>,
}

impl ResizeFrameDriver {
    pub(crate) fn new(
        app: Weak<RefCell<App>>,
        window: WindowId,
        composition: Arc<Mutex<Composition>>,
    ) -> Self {
        Self {
            app,
            window,
            composition,
        }
    }

    /// 一条 `WM_SIZE` 的副作用（见模块文档的顺序）；返回需要冒泡的粘性错误。
    pub(crate) fn frame(&self, scale: Scale, client: (u32, u32)) -> Option<EngineError> {
        let app = self.app.upgrade()?;
        // 借用冲突（帧内重入）⇒ 放弃本步：事件仍在队列里，下一条 `WM_SIZE` 会重试。
        let mut app = app.try_borrow_mut().ok()?;
        // 先收 GPU 反馈：`presented_surface` 是几何的输入，落后一帧也只是晚一步对齐。
        if let Err(error) = app.poll_renderers() {
            return Some(error);
        }
        // 0×0（`WM_SIZE` 的 SIZE_MINIMIZED 语义）不是渲染目标：模态循环里不该出现，但
        // 防御性跳过，避免把 surface 写成 0（`GetClientRect` 的最小化 0×0 在泵侧有同样的保护）。
        if client.0 == 0 || client.1 == 0 {
            return None;
        }
        let window = app.window_mut(self.window)?;
        // `set_scale` 没有同值短路（会整树失效 + 全部文本重新 shape）：只在 DPI 真的变了时写，
        // 与 `requery_display` 的口径一致。拖动中它几乎总是相同值。
        if window.scale() != scale {
            window.set_scale(scale);
        }
        window.set_size(Size::new(
            scale.to_logical(client.0 as f32),
            scale.to_logical(client.1 as f32),
        ));
        // surface = 真实客户区（逻辑尺寸取整后的 covering 值可能比它大 1 px）。
        window.set_surface(client);
        let presented = app
            .window(self.window)
            .map(Window::presented_surface)
            .unwrap_or(client);
        // 短借用之间做 DComp 调用（`Commit` 不派发消息，但保持与泵一致的纪律）。
        {
            let mut target = composition::lock(&self.composition);
            if let Err(error) = target.set_geometry(presented, client) {
                return Some(error);
            }
        }
        match app.frame(self.window) {
            Some(Ok(_)) | None => None,
            Some(Err(error)) => Some(error),
        }
    }
}
