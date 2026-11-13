//! 样式契约：静态结构 `Style` + 动态属性 `StylePatch` + `Cursor` + `Clip`。
//!
//! 设计见 `design/phase-03-node-state.md` §10 与 `design/phase-04-layout.md` §11；
//! 上游 `custom-gui-engine-design.md` §4.9（D24 / I17）。
//!
//! 分层：`Style` 是**静态结构**（挂载时一次写入），`StylePatch` 是**动态属性**（`bind_style` 每次求值重算）。
//! 引擎不提供任何链式 DSL / 宏 / `Styled` 形状的 builder（那属应用层 `base_ui`，D24）。

use smol_str::SmolStr;

use crate::{
    math::Point,
    paint::{
        color::Color,
        primitive::{Border, Gradient},
    },
};

/// 静态样式结构（I17）。
///
/// `inner` 是 taffy 的样式结构（布局属性）。**引擎不提供布局属性的访问器**：应用层的
/// `base_ui::StyleBuilder` 直接构造 `taffy::Style<SmolStr>` 再包一层即可（F-35：应用层需自行声明
/// `taffy = { workspace = true }`，版本由 workspace 精确钉住）。
///
/// P06 才会追加其余引擎自有字段（圆角 / 阴影 / 字体族）——它们现在**没有读者**，按 AGENTS §2.1 不加。
///
/// **`calc()` 不解析**：`Dimension::Calc` / `LengthPercentage::Calc` 在 v1 会按 0 处理
/// （taffy 0.15 只把 calc 当不透明指针；连它自带的 `TaffyTree` 也返回 0）。见 `design/phase-04-layout.md` §14.3。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Style {
    /// 布局属性（taffy 0.15.0，`CustomIdent = SmolStr`）。
    pub inner: taffy::Style<SmolStr>,
    /// 子树裁剪策略（P04；命中测试的 clip 链与 P06 的 `Clip` 图元同源）。
    pub clip: Clip,
}

/// 焦点环（静态结构：宽度 / 外扩 / 颜色在挂载时解析，主题切换时重挂载）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FocusRing {
    /// 环宽（逻辑像素；沿 `bounds.outset(offset)` 的内侧展开）。
    pub width: f32,
    /// 环相对节点 border box 的外扩（逻辑像素；`0.0` = 贴着节点）。
    pub offset: f32,
    /// 环颜色。
    pub color: Color,
}

impl FocusRing {
    /// 便捷构造。
    #[must_use]
    pub const fn new(width: f32, offset: f32, color: Color) -> Self {
        Self {
            width,
            offset,
            color,
        }
    }
}

/// 子树裁剪策略。
///
/// v1 只有矩形（`Bounds`）；圆角裁剪由 P06 的 `CornerRadius` 追加变体。
/// **只裁子树**（不裁本节点自己的 border/background），与 CSS `overflow: hidden` 一致。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Clip {
    /// 不裁剪。
    #[default]
    None,
    /// 按本节点的 **padding box**（布局盒内缩 `border`）裁剪整棵子树。
    Bounds,
}

/// 静态绘制属性（`design/phase-06-paint-runtime.md` §6.1）。
///
/// 与 [`Style`] 分开存放（`Node.paint`）：`Style` 内联会让 `Node` 超过 1152 B 的预算（F-58），
/// 且绘制属性与布局属性是两条独立的失效通道。动态覆盖走 [`StylePatch`]（`background`/`border`/
/// `opacity`/`foreground`），合并规则是「patch 覆盖 spec」。
///
/// `border` 的宽度是**绘制**宽度（单值、沿 `bounds` 内侧展开）；它不参与布局 —— 需要为边框留空间时
/// 请用 `Style.inner.border`（taffy 的布局边框），两者不一致时以应用的意图为准。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PaintSpec {
    /// 背景填充；`Color::TRANSPARENT`（默认）= 不画背景。
    pub background: Color,
    /// 内侧边框；`None` = 无边框。
    pub border: Option<Border>,
    /// 四角圆角（逻辑像素，CSS 顺序 `[tl, tr, br, bl]`）；编码期 clamp 到 `min(w, h) / 2`。
    pub corner_radius: [f32; 4],
    /// 解析阴影；`None` = 无阴影。
    pub shadow: Option<ShadowSpec>,
    /// 双色渐变；`None` = 实色。与 `border` 同时存在时拆成两个 `Quad`（P02 §8.5）。
    pub gradient: Option<Gradient>,
}

/// 解析阴影的静态参数（`rect` 由布局盒给出，不进样式）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShadowSpec {
    /// 偏移（逻辑像素）。
    pub offset: Point,
    /// 高斯模糊半径（逻辑像素，`>= 0`）。
    pub blur: f32,
    /// 阴影色。
    pub color: Color,
}

/// 动态属性的增量补丁（§4.9.2 的「动态属性」列）：由 `bind_style` 每次求值**重算**，值语义。
///
/// 全部字段 `Option`：`None` = 本帧不覆盖（沿用 P06 的静态兜底）。`Default` 是「空补丁」。
/// `PartialEq` 让 `refresh_styles` 能判断「补丁是否真的变了」。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StylePatch {
    /// 背景填充色（→ `Quad` 的 `paint.fill`）。
    pub background: Option<Color>,
    /// 前景色（文本/图标着色，P05 的 `GlyphRun::color`）。
    pub foreground: Option<Color>,
    /// 边框（复用 `paint::primitive::Border`：宽度 + 颜色；**不**重复定义，F-36）。
    pub border: Option<Border>,
    /// 整体不透明度（在**预乘空间**缩放，P02 §5.2 的 `Mul<f32>`）。
    pub opacity: Option<f32>,
    /// 可见性（`false` 时不绘制、不参与命中，P06/P04）。
    pub visible: Option<bool>,
    /// 命中/焦点/Action 开关（`false` = 不可交互，P10/P11）。
    pub interactive: Option<bool>,
    /// 光标形状（P10 由平台泵 `SetCursor`；`Default` 交给类光标）。
    pub cursor: Option<Cursor>,
    /// **尺寸会变时必须置 true** → `App` 追加 `LAYOUT` 并向祖先冒泡（§4.9.4 边界情况表）。
    pub affects_layout: bool,
}

/// 光标形状（P10 映射到 `IDC_*` 系统光标）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Cursor {
    /// 平台默认箭头。
    #[default]
    Default,
    /// 可点击（按钮/链接）。
    Pointer,
    /// 文本输入（I 形）。
    Text,
    /// 不允许（禁用态，§4.9.4 的例子）。
    NotAllowed,
    /// 可拖拽（分栏/画布平移）。
    Grab,
    /// 拖拽中。
    Grabbing,
    /// 水平调整大小（分栏）。
    ResizeHorizontal,
    /// 垂直调整大小（分栏）。
    ResizeVertical,
}

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;

    use super::{Clip, Cursor, Style, StylePatch};

    #[test]
    fn style_default_is_taffy_default() {
        let style = Style::default();

        assert_eq!(style.inner, taffy::Style::<SmolStr>::DEFAULT);
        assert_eq!(
            style.inner.display,
            taffy::style::Display::Flex,
            "workspace 的 taffy 开了 flexbox，Display::DEFAULT = Flex"
        );
    }

    #[test]
    fn style_patch_default_is_empty() {
        let patch = StylePatch::default();

        assert!(patch.background.is_none());
        assert!(patch.foreground.is_none());
        assert!(patch.border.is_none());
        assert!(patch.opacity.is_none());
        assert!(patch.visible.is_none());
        assert!(patch.interactive.is_none());
        assert!(patch.cursor.is_none());
        assert!(!patch.affects_layout, "affects_layout 默认必须是 false");
        assert_eq!(patch, StylePatch::default());
    }

    #[test]
    fn cursor_default_is_default() {
        assert_eq!(Cursor::default(), Cursor::Default);
        assert_ne!(Cursor::NotAllowed, Cursor::Pointer);
    }

    #[test]
    fn clip_default_is_none() {
        assert_eq!(Clip::default(), Clip::None);
        assert_eq!(Style::default().clip, Clip::None);
    }
}
