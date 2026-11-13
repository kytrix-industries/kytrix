//! 原生窗口：`NSWindow` + 自绘标题栏 + 交通灯（`design/phase-12-macos-linux.md` §7.3）。
//!
//! 状态同步（尺寸 / 缩放 / 焦点 / 关闭）走「每轮快照对比」而不是 delegate 或通知：
//! `NSWindowDelegate` 会引入第二个回调入口，而泵每轮都要读一次这些值（O(1)）。

use objc2::{MainThreadMarker, MainThreadOnly, rc::Retained};
use objc2_app_kit::{
    NSApplication, NSAutoresizingMaskOptions, NSBackingStoreType, NSColor, NSCursor, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask, NSWindowTitleVisibility,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::CAMetalLayer;

use crate::{
    app::App,
    error::EngineError,
    event::Event,
    math::{Rect, Scale, Size},
    platform::{
        dispatcher::Dispatcher,
        macos::{a11y::MacA11y, dpi, view::GuiView, wake},
    },
    window::{WindowId, WindowOptions},
};

/// 最小客户区（逻辑像素；与 Win32 / Wayland 同源）。
pub(crate) const MIN_SIZE_LOGICAL: (f32, f32) = (320.0, 200.0);

/// 一个原生窗口（`Application::run` 持有；`Drop` 时关闭）。
pub(crate) struct NativeWindow {
    mtm: MainThreadMarker,
    id: WindowId,
    window: Retained<NSWindow>,
    view: Retained<GuiView>,
    scale: Scale,
    logical_size: Size,
    /// 最近一次同步的快照（用于检测变化）。
    seen_scale: f64,
    seen_size: (f64, f64),
    was_key: bool,
    close_requested: bool,
    destroyed: bool,
    /// 最近一次下发的 IME caret 区域（逻辑坐标）。
    ime_area: Option<Rect>,
}

impl NativeWindow {
    /// 建**隐藏**窗口（不 `makeKeyAndOrderFront`）并装好 view + a11y adapter。
    pub(crate) fn create(
        mtm: MainThreadMarker,
        id: WindowId,
        options: &WindowOptions,
        dispatcher: &std::sync::Arc<Dispatcher>,
    ) -> Result<Self, EngineError> {
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable
            | NSWindowStyleMask::FullSizeContentView;
        let rect = NSRect::new(
            NSPoint::new(80.0, 80.0),
            NSSize::new(
                f64::from(options.size.width.max(1.0)),
                f64::from(options.size.height.max(1.0)),
            ),
        );
        // SAFETY: `alloc` + 指定初始化器是 NSWindow 的合法构造路径；`rect` 是栈上值。
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: 关闭时不让 AppKit 释放窗口（我们持有 `Retained`）。
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str(options.title.as_str()));
        window.setTitlebarAppearsTransparent(true);
        window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        // fullscreen 转场使用不透明窗口；透明窗口会让 AppKit 的全屏 Space 停在过渡快照上
        // （实测：view/layer 已 1680×1050，屏幕仍显示旧 960×740 快照）。引擎自己画满背景，
        // 不需要窗口透明。
        window.setOpaque(true);
        window.setBackgroundColor(Some(&NSColor::blackColor()));
        window.setAcceptsMouseMovedEvents(true);
        window.setContentMinSize(NSSize::new(
            f64::from(MIN_SIZE_LOGICAL.0),
            f64::from(MIN_SIZE_LOGICAL.1),
        ));
        // 显式声明该窗口是原生全屏的 primary 窗口；默认 collectionBehavior=0 时
        // AppKit 的 fullscreen 转场会停在过渡快照上（实测：窗口/view/layer 都已
        // 1680×1050，但屏幕仍显示旧的 960×740 快照）。
        window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenPrimary);

        let view = GuiView::new(mtm);
        window.setContentView(Some(&view));
        // 原生全屏转场由 AppKit 直接改 window frame；contentView 必须随宽度/高度伸缩，
        // 否则引擎看到的一直是 960×740，layer/drawable 也不会覆盖全屏窗口。
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        view.setWantsLayer(true);
        window.makeFirstResponder(Some(&view));

        // 原生交通灯全部保留：关闭 / 最小化 / 缩放。标题栏透明且标题隐藏，内容铺满窗口，
        // 因此窗口控件浮在内容上方（应用用顶部安全区避让）。
        let a11y = MacA11y::new(&view, dispatcher.clone(), id)?;
        view.install_a11y(a11y);

        let scale = f64::from(options.scale.factor());
        let mut native = Self {
            mtm,
            id,
            window,
            view,
            scale: options.scale,
            logical_size: options.size,
            seen_scale: 1.0,
            seen_size: (0.0, 0.0),
            was_key: false,
            close_requested: false,
            destroyed: false,
            ime_area: None,
        };
        native.view.set_scale(scale);
        native.sync_initial(mtm)?;
        Ok(native)
    }

    /// 初次同步：读 `backingScaleFactor` / 内容尺寸，写回字段（**不**产生事件）。
    fn sync_initial(&mut self, _mtm: MainThreadMarker) -> Result<(), EngineError> {
        let factor = self.window.backingScaleFactor();
        let scale = dpi::scale_from(factor)?;
        self.scale = scale;
        self.seen_scale = factor;
        self.view.set_scale(factor);
        let size = self.view.frame().size;
        self.seen_size = (size.width, size.height);
        self.logical_size = Size::new(size.width as f32, size.height as f32);
        Ok(())
    }

    /// 把渲染器的 `CAMetalLayer` 挂到 view 上（`wantsLayer` 已在 `create` 里打开）。
    ///
    /// 只在这里把 layer 交给 AppKit；之后的 `contentsScale` / `drawableSize` 全部由 render
    /// 线程写（主线程再写会与 `nextDrawable` 竞争）。
    pub(crate) fn attach_layer(&self, layer: Retained<CAMetalLayer>) {
        self.view.setLayer(Some(&layer));
    }

    pub(crate) fn show(&self) {
        self.window.makeKeyAndOrderFront(None);
        self.window.makeFirstResponder(Some(&self.view));
    }

    pub(crate) fn id(&self) -> WindowId {
        self.id
    }

    pub(crate) fn scale(&self) -> Scale {
        self.scale
    }

    pub(crate) fn logical_size(&self) -> Size {
        self.logical_size
    }

    /// 物理像素尺寸（建渲染器 / resize 的唯一输入）。
    pub(crate) fn physical_size(&self) -> (u32, u32) {
        dpi::physical_size(self.logical_size, self.scale)
    }

    /// 取走本轮平台事件。
    pub(crate) fn take_events(&self) -> Vec<Event> {
        self.view.take_events()
    }

    /// 每轮对比窗口状态，按需产生 `DpiChanged` / `Resized` / `FocusChanged` / `CloseRequested`。
    pub(crate) fn sync_state(
        &mut self,
        app: &mut App,
        dispatcher: &std::sync::Arc<Dispatcher>,
    ) -> Result<(), EngineError> {
        let mut events: Vec<Event> = Vec::new();
        let factor = self.window.backingScaleFactor();
        if (factor - self.seen_scale).abs() > f64::EPSILON {
            let scale = dpi::scale_from(factor)?;
            self.seen_scale = factor;
            self.scale = scale;
            self.view.set_scale(factor);
            if let Some(window) = app.window_mut(self.id) {
                window.set_scale(scale);
            }
            events.push(Event::DpiChanged {
                dpi: dpi::dpi_from_scale(scale),
                scale,
            });
        }
        let size = self.view.frame().size;
        if (size.width - self.seen_size.0).abs() > 0.5
            || (size.height - self.seen_size.1).abs() > 0.5
        {
            self.seen_size = (size.width, size.height);
            let logical = Size::new(size.width as f32, size.height as f32);
            self.logical_size = logical;
            if let Some(window) = app.window_mut(self.id) {
                window.set_size(logical);
            }
            events.push(Event::Resized { size: logical });
        }
        let key = self.window.isKeyWindow();
        if key != self.was_key {
            self.was_key = key;
            self.view.focus_changed(key);
            events.push(Event::FocusChanged { focused: key });
        }
        // 关闭判定只看「窗口真的不在了」：最小化（`isMiniaturized`）与 `NSApp.hide()`
        // 都会让 `isVisible` 变 false，但窗口并没有被关闭。
        let minimized = self.window.isMiniaturized();
        let app_hidden = NSApplication::sharedApplication(self.mtm).isHidden();
        let closed = !self.window.isVisible() && !minimized && !app_hidden;
        if !closed {
            self.close_requested = false;
        } else if !self.destroyed && !self.close_requested {
            self.close_requested = true;
            events.push(Event::CloseRequested);
        }
        for event in events {
            app.dispatch(self.id, event);
            dispatcher.wake();
        }
        Ok(())
    }

    /// 交付一笔 a11y 更新（`a11y_pending` 为 O(1) 过滤；adapter 不活跃时闭包不执行）。
    pub(crate) fn update_a11y(&mut self, app: &mut App) {
        if !app.a11y_pending(self.id) {
            return;
        }
        let id = self.id;
        self.view
            .update_a11y(|| crate::a11y::adapter::build_update(app, id));
    }

    /// 同步 IME caret 区域到 `NSTextInputClient` 的 `firstRect`（每轮泵调用，独立于帧门禁）。
    pub(crate) fn sync_ime_cursor_area(&mut self, app: &App) {
        let area = match app.ime_cursor_area(self.id) {
            Ok(area) => area,
            // 文本尚未 shape / 文本刚变化：保留上一次有效区域。
            Err(_) => return,
        };
        if self.ime_area == area {
            return;
        }
        self.ime_area = area;
        self.view.set_ime_cursor_area(area);
    }

    /// 关闭窗口（幂等）。
    pub(crate) fn close(&mut self) {
        if self.destroyed {
            return;
        }
        self.window.close();
        self.destroyed = true;
    }

    /// 取走「关闭已请求」标志（泵唯一读点）。
    pub(crate) fn take_close_request(&mut self) -> bool {
        std::mem::take(&mut self.close_requested)
    }

    pub(crate) fn is_destroyed(&self) -> bool {
        self.destroyed
    }

    /// 光标形状（v1 只支持系统箭头；`take_cursor_change` 的其余载荷等 P13 接）。
    pub(crate) fn set_cursor(&self) {
        NSCursor::arrowCursor().set();
    }

    /// 动画定时器的开关（`pump` 每轮调用；空闲时移除定时器 ⇒ 不轮询）。
    pub(crate) fn set_animating(&self, active: bool) {
        wake::set_animating(active, 1.0 / 60.0);
    }
}

impl Drop for NativeWindow {
    fn drop(&mut self) {
        self.close();
    }
}
