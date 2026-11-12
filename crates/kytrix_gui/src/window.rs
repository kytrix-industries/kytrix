//! 窗口：句柄、建窗参数、节点树 + 绘制场景的持有者。
//!
//! 设计见 `design/phase-03-node-state.md` §5.2 / §15。P03 是 headless 的：只保存参数，不驱动平台
//! （平台窗口句柄、DPI 事件、标题栏属 P12）。

use smol_str::SmolStr;

use crate::{
    error::EngineError,
    layout::LayoutReport,
    math::{Scale, Size},
    node::tree::NodeTree,
    paint::scene::Scene,
};

/// 窗口句柄：单调递增、**永不复用**（陈旧 id 落回 `None`，因此不需要代际）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WindowId(u32);

impl WindowId {
    /// crate 内构造（消费者：`App::create_window`）。
    ///
    /// **0 不是合法句柄**：`App::create_window` 从 1 起单调递增，`WindowId(0)` 永不产生
    /// （`Option<WindowId>` 因此可以靠 `get` 返回 `None` 表达「窗口不存在」，无需哨兵值）。
    pub(crate) fn from_raw(value: u32) -> Self {
        debug_assert!(value != 0, "WindowId 0 是非法句柄");
        Self(value)
    }

    /// 原始值（诊断/日志）。
    #[must_use]
    pub const fn to_bits(self) -> u32 {
        self.0
    }

    /// 仅测试：直接构造。
    #[cfg(test)]
    pub(crate) const fn for_test(value: u32) -> Self {
        Self(value)
    }
}

/// 建窗参数。P03 只保存，不驱动平台（P12 才消费）。
#[derive(Clone, Debug, PartialEq)]
pub struct WindowOptions {
    /// 标题（`SmolStr`，AGENTS §2.2）。
    pub title: SmolStr,
    /// 初始逻辑尺寸。
    pub size: Size,
    /// 初始 DPI 比例（由平台在 P12 传入；headless 默认 `Scale::ONE`）。
    pub scale: Scale,
}

impl Default for WindowOptions {
    fn default() -> Self {
        Self {
            title: SmolStr::new_static(""),
            size: Size::new(800.0, 600.0),
            scale: Scale::ONE,
        }
    }
}

/// 一个窗口的全部状态：节点树 + 绘制场景 + DPI + 参数。
pub struct Window {
    id: WindowId,
    options: WindowOptions,
    tree: NodeTree,
    scene: Scene,
    scale: Scale,
}

impl Window {
    /// **唯一的构造点**：`Scene::empty()` 与 `NodeTree::new()` 只在这里组合。
    pub(crate) fn new(id: WindowId, options: WindowOptions) -> Self {
        let scale = options.scale;
        Self {
            id,
            options,
            tree: NodeTree::new(),
            scene: Scene::empty(),
            scale,
        }
    }

    /// 窗口句柄。
    #[must_use]
    pub fn id(&self) -> WindowId {
        self.id
    }

    /// 节点树只读视图。
    #[must_use]
    pub fn tree(&self) -> &NodeTree {
        &self.tree
    }

    /// 应用层建树的唯一入口（`NodeBuilder::build` 的 `&mut NodeTree` 来自这里）。
    pub fn tree_mut(&mut self) -> &mut NodeTree {
        &mut self.tree
    }

    /// 只读场景（S-10 / S-33：应用层**拿不到** `&mut Scene`，`Scene` 也**没有**公开构造器）。
    #[must_use]
    pub fn scene(&self) -> &Scene {
        &self.scene
    }

    /// 建窗参数快照。
    #[must_use]
    pub fn options(&self) -> &WindowOptions {
        &self.options
    }

    /// 当前生效的 DPI 比例。
    #[must_use]
    pub fn scale(&self) -> Scale {
        self.scale
    }

    /// 窗口逻辑尺寸（根布局的可用空间）。
    #[must_use]
    pub fn size(&self) -> Size {
        self.options.size
    }

    /// 逻辑尺寸变化（消费者：P12 的 resize 事件）：写尺寸 + 标根 `LAYOUT` + 清根缓存。
    pub fn set_size(&mut self, size: Size) {
        self.options.size = size;
        let root = self.tree.root();
        self.tree.mark_layout(root);
    }

    /// 用窗口逻辑尺寸重排，返回本次布局统计（P06/P09 的帧驱动调用它）。
    ///
    /// # Errors
    /// `EngineError::InvalidMeasure { node, axis, value }`：某个 measure 闭包返回 NaN / 负数（首条优先）。
    /// 出错时布局盒可能为空，调用方**不得**提交本帧。
    pub fn compute_layout(&mut self) -> Result<LayoutReport, EngineError> {
        self.tree.compute_layout(taffy::geometry::Size {
            width: Some(self.options.size.width),
            height: Some(self.options.size.height),
        })
    }

    /// DPI 变化（P12 的事件处理调用它）：更新比例并标根 `LAYOUT | PAINT`。
    ///
    /// **逻辑布局盒与 scale 无关**（I11：布局全用逻辑像素，物理化在 paint/P06），这里只保守地失效。
    ///
    /// **不**置 `App` 的唤醒请求：DPI 事件本身来自平台，平台总是会调度一帧（P12）。
    pub fn set_scale(&mut self, scale: Scale) {
        self.scale = scale;
        let root = self.tree.root();
        self.tree.mark_layout(root);
    }
}

#[cfg(test)]
mod tests {
    use super::{Window, WindowId, WindowOptions};
    use crate::{
        math::{Scale, Size},
        node::dirty::DirtyFlags,
    };

    fn window() -> Window {
        Window::new(WindowId::for_test(1), WindowOptions::default())
    }

    #[test]
    fn new_window_has_root_and_empty_scene() {
        let window = window();

        assert_eq!(window.id(), WindowId::for_test(1));
        assert_eq!(window.tree().order().len(), 1, "初始只有 root");
        assert_eq!(window.scene().lane_count(), 0);
        assert!(window.scene().is_empty());
        assert_eq!(window.scale(), Scale::ONE);
    }

    #[test]
    fn scene_starts_without_lanes() {
        let window = window();
        let scene = window.scene();

        assert_eq!(scene.lane_count(), 0);
        assert!(scene.order().is_empty());
        assert_eq!(scene.primitive_count(), 0);
        assert!(scene.is_empty());
    }

    #[test]
    fn set_scale_marks_root_layout() {
        let mut window = window();
        window.tree_mut().clear_all_dirty_for_test();
        let scale = Scale::new(2.0).expect("合法比例");

        window.set_scale(scale);

        assert_eq!(window.scale(), scale);
        let root = window.tree().root();
        let flags = window.tree().get(root).expect("root 存在").dirty();
        assert!(flags.contains(DirtyFlags::LAYOUT));
        assert!(flags.contains(DirtyFlags::PAINT));
    }

    #[test]
    fn window_options_default_values() {
        let options = WindowOptions::default();

        assert_eq!(options.title, "");
        assert_eq!(options.size.width, 800.0);
        assert_eq!(options.size.height, 600.0);
        assert_eq!(options.scale, Scale::ONE);
    }

    #[test]
    fn window_size_is_root_available_space() {
        use crate::node::builder::NodeBuilder;

        let mut window = Window::new(
            WindowId::for_test(1),
            WindowOptions {
                size: Size::new(120.0, 60.0),
                ..WindowOptions::default()
            },
        );
        let root = window.tree().root();
        let child = NodeBuilder::boxed()
            .build(window.tree_mut(), root)
            .expect("挂载成功");

        let report = window.compute_layout().expect("布局成功");

        assert_eq!(window.size(), Size::new(120.0, 60.0));
        assert_eq!(
            window.tree().get(root).expect("root").layout().bounds.size,
            Size::new(120.0, 60.0),
            "根 100% × 100% 撑满窗口"
        );
        assert!(report.laid_out >= 2, "根与子节点都要布局：{report:?}");
        let _ = child;
    }

    #[test]
    fn dpi_scale_does_not_change_logical_boxes() {
        use crate::{
            math::Rect,
            node::builder::NodeBuilder,
            paint::style::{Clip, Style},
        };

        fn fixed(width: f32, height: f32) -> Style {
            let mut inner = taffy::Style::<smol_str::SmolStr>::DEFAULT;
            inner.size = taffy::geometry::Size {
                width: taffy::style::Dimension::length(width),
                height: taffy::style::Dimension::length(height),
            };
            Style {
                inner,
                clip: Clip::None,
            }
        }

        let mut window = Window::new(
            WindowId::for_test(1),
            WindowOptions {
                size: Size::new(120.0, 60.0),
                ..WindowOptions::default()
            },
        );
        let root = window.tree().root();
        let child = NodeBuilder::boxed()
            .style(fixed(10.5, 3.25))
            .build(window.tree_mut(), root)
            .expect("挂载成功");
        let _ = window.compute_layout().expect("布局成功");
        let before = window.tree().get(child).expect("存在").layout().bounds;

        window.set_scale(Scale::new(1.5).expect("合法比例"));
        let _ = window.compute_layout().expect("布局成功");

        assert_eq!(
            window.tree().get(child).expect("存在").layout().bounds,
            before,
            "逻辑布局盒与 scale 无关（物理化在 P06）"
        );
        assert_eq!(before, Rect::new(0.0, 0.0, 10.5, 3.25));
    }

    #[test]
    fn resize_relayouts_percent_children() {
        use crate::{
            node::builder::NodeBuilder,
            paint::style::{Clip, Style},
        };

        let mut window = Window::new(
            WindowId::for_test(1),
            WindowOptions {
                size: Size::new(100.0, 100.0),
                ..WindowOptions::default()
            },
        );
        let root = window.tree().root();
        let mut inner = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::percent(1.0),
            height: taffy::style::Dimension::percent(1.0),
        };
        let child = NodeBuilder::boxed()
            .style(Style {
                inner,
                clip: Clip::None,
            })
            .build(window.tree_mut(), root)
            .expect("挂载成功");
        let _ = window.compute_layout().expect("布局成功");
        assert_eq!(
            window.tree().get(child).expect("存在").layout().bounds.size,
            Size::new(100.0, 100.0)
        );

        window.set_size(Size::new(300.0, 50.0));
        let _ = window.compute_layout().expect("布局成功");

        assert_eq!(
            window.tree().get(child).expect("存在").layout().bounds.size,
            Size::new(300.0, 50.0),
            "百分比尺寸随根可用空间变化"
        );
    }

    #[test]
    fn set_size_marks_root_layout_and_clears_cache() {
        use crate::node::builder::NodeBuilder;

        let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
        let root = window.tree().root();
        let _ = NodeBuilder::boxed()
            .build(window.tree_mut(), root)
            .expect("挂载成功");
        let _ = window.compute_layout().expect("布局成功");

        window.set_size(Size::new(640.0, 480.0));

        let flags = window.tree().get(root).expect("root 存在").dirty();
        assert!(
            flags.contains(DirtyFlags::LAYOUT),
            "尺寸变化必须标根 LAYOUT（实际 {flags:?}）"
        );
        assert_eq!(window.size(), Size::new(640.0, 480.0));
    }
}
