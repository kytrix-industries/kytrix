//! 样式契约：静态结构 `Style` + 动态属性 `StylePatch` + `Cursor`。
//!
//! 设计见 `design/phase-03-node-state.md` §10；上游 `custom-gui-engine-design.md` §4.9（D24 / I17）。
//!
//! 分层：`Style` 是**静态结构**（挂载时一次写入），`StylePatch` 是**动态属性**（`bind_style` 每次求值重算）。
//! 引擎不提供任何链式 DSL / 宏 / `Styled` 形状的 builder（那属应用层 `base_ui`，D24）。

use smol_str::SmolStr;

use crate::paint::{color::Color, primitive::Border};

/// 静态样式结构（I17）。
///
/// `inner` 是 taffy 的样式结构（布局属性）。**引擎不提供布局属性的访问器**：应用层的
/// `base_ui::StyleBuilder` 直接构造 `taffy::Style<SmolStr>` 再包一层即可（F-35：应用层需自行声明
/// `taffy = { workspace = true }`，版本由 workspace 精确钉住）。
///
/// P06 才会追加引擎自有字段（圆角 / clip / 阴影 / 字体族）——它们现在**没有读者**，按 AGENTS §2.1 不加。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Style {
    /// 布局属性（taffy 0.15.0，`CustomIdent = SmolStr`）。
    pub inner: taffy::Style<SmolStr>,
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
    /// 光标形状（P12 设置平台光标）。
    pub cursor: Option<Cursor>,
    /// **尺寸会变时必须置 true** → `App` 追加 `LAYOUT` 并向祖先冒泡（§4.9.4 边界情况表）。
    pub affects_layout: bool,
}

/// 光标形状（P12 映射到平台光标）。
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

    use super::{Cursor, Style, StylePatch};

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
}
