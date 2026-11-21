//! 样式契约：静态结构 `Style` + 动态属性 `StylePatch` + `Cursor` + `Clip`。
//!
//! 上游 `custom-gui-engine-design.md` §4.9（D24 / I17）。
//!
//! 分层：`Style` 是**静态结构**（挂载时一次写入），`StylePatch` 是**动态属性**（`bind_style` 每次求值重算）。
//! 引擎不提供任何链式 DSL / 宏 / `Styled` 形状的 builder（那属应用层 `base_ui`，D24）。

use smallvec::SmallVec;
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
/// （taffy 0.15 只把 calc 当不透明指针；连它自带的 `TaffyTree` 也返回 0）。
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

/// 静态绘制属性。
///
/// 与 [`Style`] 分开存放（`Node.paint`）：绘制属性与布局属性是两条独立的失效通道（F-58）；
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
    /// 多层解析阴影（**按序绘制**：先画的在下，后画的覆盖在上；空 = 无阴影，E13）。
    pub shadows: Shadows,
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
    /// 扩张（逻辑像素；CSS `box-shadow` 的 `spread-radius`）。
    ///
    /// paint 期折算进图元：投影盒四边外扩该值、圆角同量增加（负值收缩，半径 clamp 到 `0`）。
    /// σ 不随 spread 变，**不进** `GpuPrimitive` / shader —— 三端与字节契约零改动（E13）。
    pub spread: f32,
    /// 阴影色。
    pub color: Color,
}

/// 阴影层数上限：构造期截断（`Shadows::new` / `From<ShadowSpec>`），渲染层不再判上限。
///
/// 4 = 旧 GPUI 主题里最多的组合（菜单 2 层 + 焦点环阴影 + 浮层 1 层）再留 1 层余量；
/// 每层 = 1 个 `Primitive::Shadow`（1 个实例 draw），层数直接等于额外的 draw 数。
pub const MAX_SHADOWS: usize = 4;

/// 多层解析阴影（[`PaintSpec::shadows`] 的类型）。**按序绘制**：数组序 = 绘制序。
///
/// 内联容量 2：旧菜单的双层阴影、dialog/浮层的单层阴影都不分配；3–4 层才上堆。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Shadows(SmallVec<[ShadowSpec; 2]>);

impl Shadows {
    /// 从迭代器构造；超过 [`MAX_SHADOWS`] 的层被**截断丢弃**（构造期一次性判定，渲染层无分支）。
    pub fn new(layers: impl IntoIterator<Item = ShadowSpec>) -> Self {
        Self(layers.into_iter().take(MAX_SHADOWS).collect())
    }

    /// 层数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// 是否没有阴影。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// 绘制序（数组序）迭代。
    pub fn iter(&self) -> std::slice::Iter<'_, ShadowSpec> {
        self.0.iter()
    }
}

impl From<ShadowSpec> for Shadows {
    fn from(layer: ShadowSpec) -> Self {
        Self(SmallVec::from_slice(&[layer]))
    }
}

impl FromIterator<ShadowSpec> for Shadows {
    /// 同 [`Shadows::new`]：超过 [`MAX_SHADOWS`] 的层被丢弃。
    fn from_iter<I: IntoIterator<Item = ShadowSpec>>(iter: I) -> Self {
        Self::new(iter)
    }
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
    /// 不透明度（在**预乘空间**缩放，P02 §5.2 的 `Mul<f32>`）。
    ///
    /// **语义是「组不透明度」（E13）**：本节点的有效 opacity = 自身值 × 祖先链上全部值，再乘到
    /// 本节点与整棵子树的**每个图元**上（父 0.5 × 子 0.5 ⇒ 子图元 0.25）。给组根节点绑一个值即
    /// 可整组淡入淡出：配 `Transient` + `App::animate` 时 `nodes` 只需登记该组根节点，子树随父
    /// 级联自动重绘。
    ///
    /// 与 CSS `opacity` 的差异（**已知语义残余**）：没有离屏合成的「隔离组」语义 —— 相互重叠的
    /// 子图元各自乘 alpha 后在目标上直接混合，重叠区会比真正的组不透明更实（半透明边框叠在
    /// 背景上尤其明显）；不重叠的子树与 CSS 逐像素一致。
    ///
    /// 它**不**作用于 `Canvas` 内容（应用自绘、引擎不解析其颜色）；`GeometryDraw.color` 由引擎
    /// 给定，**会**被乘上。有效值变化会让整棵子树重建 lane（复用判据含 opacity 戳），稳定帧
    /// 零重建（I3）。
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
    use std::mem::size_of;

    use smol_str::SmolStr;

    use super::{Clip, Cursor, MAX_SHADOWS, PaintSpec, ShadowSpec, Shadows, Style, StylePatch};
    use crate::{math::Point, paint::color::Color};

    fn layer(offset_y: f32) -> ShadowSpec {
        ShadowSpec {
            offset: Point::new(0.0, offset_y),
            blur: 4.0,
            spread: 0.0,
            color: Color::BLACK,
        }
    }

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

    /// E13：空 `Shadows` 是默认值（`PaintSpec::default()` 不画阴影）。
    #[test]
    fn paint_spec_default_has_no_shadows() {
        let paint = PaintSpec::default();
        assert!(paint.shadows.is_empty());
        assert_eq!(paint.shadows, Shadows::default());
    }

    /// E13：单层入口（`From<ShadowSpec>`）保留那一层，`iter()` 的序 = 绘制序。
    #[test]
    fn a_single_layer_roundtrips_through_from() {
        let shadows = Shadows::from(layer(2.0));
        assert_eq!(shadows.len(), 1);
        assert!(!shadows.is_empty());
        assert_eq!(
            shadows.iter().copied().collect::<Vec<_>>(),
            vec![layer(2.0)]
        );
    }

    /// E13：构造期按 [`MAX_SHADOWS`] 截断（顺序保留，超出部分丢弃，渲染层不再判上限）。
    #[test]
    fn shadows_truncate_at_the_layer_cap() {
        let layers: Vec<ShadowSpec> = (0..MAX_SHADOWS as i32 + 2)
            .map(|index| layer(index as f32))
            .collect();
        let shadows = Shadows::new(layers.clone());
        assert_eq!(shadows.len(), MAX_SHADOWS, "上限 {MAX_SHADOWS} 层");
        assert_eq!(
            shadows.iter().copied().collect::<Vec<_>>(),
            layers[..MAX_SHADOWS].to_vec(),
            "按数组序保留前 MAX_SHADOWS 层（绘制序 = 层序）"
        );

        // `FromIterator` 与 `new` 同一口径（防止两条构造路径漂移）。
        let collected: Shadows = layers.iter().copied().collect();
        assert_eq!(collected, shadows);
    }

    /// E13：`Shadows` 的体积预算 —— 常见 1–2 层不分配（内联），且 `PaintSpec` 装在
    /// `Option<Box<PaintSpec>>` 里（`Node` 不因阴影层增长）。
    ///
    /// 审查 F6：spec-delta E13 §2.4 的尺寸表是**精确基线**（不是上界），这里逐项 pin 住
    /// （336 / 192 / 88 / 36）；任何布局变化都要同步 E13.md 与 design §11.2。
    #[test]
    fn shadows_size_budget() {
        let single = size_of::<ShadowSpec>();
        let shadows = size_of::<Shadows>();
        let paint = size_of::<PaintSpec>();
        println!("size_of::<ShadowSpec>() = {single}");
        println!("size_of::<Shadows>() = {shadows}");
        println!("size_of::<PaintSpec>() = {paint}");
        assert_eq!(single, 36, "ShadowSpec 尺寸基线（E13 §2.4）");
        assert_eq!(
            shadows, 88,
            "Shadows 尺寸基线（内联 2 层 = ShadowSpec 36×2 + SmallVec 头 16）"
        );
        assert_eq!(
            paint, 192,
            "PaintSpec 尺寸基线（E13 §2.4；装在 `Option<Box<...>>` 里，不影响 `Node` 336 B）"
        );
    }
}
