//! a11y 直连：`accesskit_unix::Adapter`（AT-SPI over D-Bus）。
//!
//! # 坐标系（跨进程 AT-SPI 的绝对坐标契约）
//!
//! AT-SPI 的 `Extents`/`GetAccessibleAtPoint` 是**屏幕绝对坐标**：`accesskit_atspi_common`
//! 的 `WindowBounds` 把节点 bounds 直接加上 `inner.origin()`
//! （`accesskit_atspi_common-0.21/src/util.rs` 的 `accesskit_point_to_atspi_point`），
//! **不做任何缩放 / 平移转换**。由此得到引擎侧的两条硬要求：
//!
//! 1. **节点 bounds 必须是物理像素、窗口客户区相对** —— `a11y::mapping::bounds_of` 已经是
//!    （逻辑布局 × `Scale`，逐边 round）；
//! 2. **`set_root_window_bounds` 的 `inner`/`outer` 必须是物理像素、屏幕绝对** ——
//!    `inner.origin()` 就是 AT-SPI 坐标系里"窗口客户区左上角在屏幕上的位置"。
//!
//! **Wayland 的协议限制**：`xdg_toplevel` 不暴露窗口的全局位置（只有尺寸与状态），所以
//! Wayland 下引擎拿不到屏幕原点。此时按 GTK/GNOME 在 Wayland 上的同一约定退化：
//! `inner.origin()` 传 `(0, 0)`，AT-SPI 拿到的 extent 就是**窗口客户区相对**的（Orca 在
//! Wayland 下对 GTK 应用也是同样的观察）。这不是"静默错误"：坐标空间在
//! [`window_bounds`] 的文档里写死，平台若将来给出位置（X11 后端 / 合成器扩展），把
//! `origin` 传进来即得到真正的屏幕绝对像素 —— 转换只有这一处。
//!
//! 与 Windows / macOS 的差异（F-124）：`update_if_active` **无返回值**；adapter 自己起 tokio
//! 任务与 D-Bus 注册（`accesskit_unix` 的 `tokio` feature），因此 `new` 必须在 runtime 内调用。

use std::sync::Arc;

use accesskit::{ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler, TreeUpdate};
use accesskit_unix::Adapter;

use crate::{
    error::EngineError,
    math::{Point, Scale, Size},
    platform::dispatcher::{Dispatcher, UserTask},
    window::WindowId,
};

/// 窗口的 a11y 状态（`event_loop` 持有；主线程独占）。
pub(crate) struct UnixA11y {
    adapter: Adapter,
}

impl UnixA11y {
    /// 建 adapter（D-Bus 注册在 adapter 内部异步完成）。
    ///
    /// # Errors
    /// 当前不产生错误（`accesskit_unix` 的注册失败只记录诊断，不影响渲染）。
    pub(crate) fn new(dispatcher: Arc<Dispatcher>, window: WindowId) -> Result<Self, EngineError> {
        let actions = UnixActions { dispatcher, window };
        Ok(Self {
            adapter: Adapter::new(UnixActivation, actions, UnixDeactivation),
        })
    }

    /// 交付一笔增量更新（闭包只在 adapter 活跃时执行 ⇒ 懒激活零开销）。
    pub(crate) fn update(&mut self, factory: impl FnOnce() -> TreeUpdate) {
        self.adapter.update_if_active(factory);
    }

    /// 更新窗口在屏幕上的几何（**物理像素**）。
    ///
    /// `origin` 是客户区左上角的屏幕绝对位置；Wayland 拿不到时传 `Point::ZERO`
    /// （坐标空间说明见模块文档）。
    pub(crate) fn set_bounds(&mut self, origin: Point, size: Size, scale: Scale) {
        let (outer, inner) = window_bounds(origin, size, scale);
        self.adapter.set_root_window_bounds(outer, inner);
    }

    pub(crate) fn focus_changed(&mut self, focused: bool) {
        self.adapter.update_window_focus_state(focused);
    }
}

/// 激活：首棵全树由主线程在下一轮交付（`None` ⇒ AccessKit 先给 placeholder 根）。
struct UnixActivation;

impl ActivationHandler for UnixActivation {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        None
    }
}

/// 停用：adapter 被对端关闭时回调（引擎没有需要清理的 a11y 状态）。
struct UnixDeactivation;

impl DeactivationHandler for UnixDeactivation {
    fn deactivate_accessibility(&mut self) {}
}

/// Action 回投：**只入队**（adapter 的 D-Bus 回调线程 ⇒ 主线程的 `Dispatcher::push`）。
struct UnixActions {
    dispatcher: Arc<Dispatcher>,
    window: WindowId,
}

impl ActionHandler for UnixActions {
    fn do_action(&mut self, request: ActionRequest) {
        let window = self.window;
        self.dispatcher.push(UserTask::new(move |app| {
            crate::a11y::action::dispatch(app, window, request);
        }));
    }
}

/// 窗口几何 → accesskit 的 `outer`/`inner` 矩形（**物理像素**；`inner` = 客户区）。
///
/// `accesskit_unix` 只把 `inner.origin()` 加到节点 bounds 上（不缩放、不转换），所以：
/// - 尺寸必须先从逻辑像素换算成**覆盖取整**的物理像素（见下方注释：不能用 buffer 尺寸的 round 规则）；
/// - `origin` 必须是客户区左上角的屏幕绝对物理像素；未知时传 `Point::ZERO`
///   （Wayland，见模块文档）。
#[must_use]
pub(crate) fn window_bounds(
    origin: Point,
    size: Size,
    scale: Scale,
) -> (accesskit::Rect, accesskit::Rect) {
    // 注意：**不能**用 [`dpi::physical_size`]。那条规则是 `wp_fractional_scale_v1` 的
    // **buffer 尺寸**语义（`round(logical × scale)`，避免 compositor 重采样），而 AT-SPI 的
    // `BoundingRectangle` 必须**覆盖实际绘制的像素**：取整方向必须是 round-out（ceil），
    // 否则 401×1.25 = 501.25 会报 501，比真实绘制的第 502 列少 1 px，读屏的命中/高亮框会偏。
    // 这与布局层「只吸附边界、向外取整」是同一条规则。
    let scale_factor = f64::from(scale.factor());
    let width = (f64::from(size.width) * scale_factor).ceil().max(1.0);
    let height = (f64::from(size.height) * scale_factor).ceil().max(1.0);
    let x = f64::from(origin.x);
    let y = f64::from(origin.y);
    let rect = accesskit::Rect {
        x0: x,
        y0: y,
        x1: x + width,
        y1: y + height,
    };
    // 客户端装饰（CSD）不存在时 outer == inner：引擎没有系统装饰，两者一致。
    (rect, rect)
}

#[cfg(test)]
mod tests {
    use super::window_bounds;
    use crate::math::{Point, Scale, Size};

    fn scale(factor: f32) -> Scale {
        Scale::new(factor).expect("scale")
    }

    /// AT-SPI 的坐标必须是**物理像素**：逻辑尺寸 400×300、scale 2 ⇒ 800×600。
    #[test]
    fn window_bounds_are_physical_pixels() {
        let (outer, inner) = window_bounds(Point::ZERO, Size::new(400.0, 300.0), scale(2.0));
        assert_eq!(
            (inner.x0, inner.y0, inner.x1, inner.y1),
            (0.0, 0.0, 800.0, 600.0)
        );
        assert_eq!(outer, inner, "无系统装饰时 outer == inner");
    }

    /// 分数缩放（1.25）按"覆盖取整"换算（与布局/绘制同一规则）。
    #[test]
    fn fractional_scale_is_rounded_up() {
        let (_, inner) = window_bounds(Point::ZERO, Size::new(400.0, 300.0), scale(1.25));
        assert_eq!((inner.x1, inner.y1), (500.0, 375.0));
        let (_, odd) = window_bounds(Point::ZERO, Size::new(401.0, 301.0), scale(1.25));
        assert_eq!(
            (odd.x1, odd.y1),
            (502.0, 377.0),
            "401×1.25 = 501.25 ⇒ 覆盖取整 502"
        );
    }

    /// 屏幕原点必须原样透传（AT-SPI 的可见坐标 = inner.origin + 节点 bounds）。
    #[test]
    fn the_screen_origin_is_passed_through() {
        let (_, inner) = window_bounds(Point::new(120.0, 48.0), Size::new(100.0, 50.0), Scale::ONE);
        assert_eq!((inner.x0, inner.y0), (120.0, 48.0));
        assert_eq!((inner.x1, inner.y1), (220.0, 98.0));
    }
}
