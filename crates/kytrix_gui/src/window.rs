//! 窗口：句柄、建窗参数、节点树 + 绘制场景的持有者。
//!
//! 设计见 `design/phase-03-node-state.md` §5.2 / §15。P03 是 headless 的：只保存参数，不驱动平台
//! （平台窗口句柄、DPI 事件、标题栏属 P12）。

use smol_str::SmolStr;

use crate::{
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

    /// DPI 变化（P12 的事件处理调用它）：更新比例并标根 `LAYOUT | PAINT`。
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
    use crate::{math::Scale, node::dirty::DirtyFlags};

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
}
