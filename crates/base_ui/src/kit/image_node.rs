//! `icon` / `brand` 共用的图片节点挂载（crate 私有）。
//!
//! # 为什么是「行内盒图片」
//!
//! 引擎没有图片节点类型：`NodeKind` 只有 `Box` / `Text` / `Canvas` / `Geometry`，`PaintSpec`
//! 也没有图片字段（`paint/style.rs`）。唯一的**参与布局**的图片通道是 E2 的行内盒图片
//! （`InlineBoxPaint::Image`，设计 §11.2）：文本节点里放一个行内盒，paint 期由引擎把
//! `ResourceId` 解析成图集 tile，并按行内盒落点画 `Primitive::Image { modulation }` ——
//! 位置与尺寸都由引擎算，应用只给逻辑尺寸。
//!
//! `Canvas` 也能画图片，但它的图元坐标是**绝对窗口坐标**（`paint/primitive.rs` 契约），
//! 挂载期拿不到父节点的最终布局位置 ⇒ 不能作为通用组件的图片通道。
//!
//! # 为什么要一个空格 + 固定尺寸（引擎限制的绕行）
//!
//! 引擎的文本路径有两处「空文本 = 什么都没有」的硬编码（`text/shape.rs`）：
//! `ShapedText::measure` 对空文本直接返回 `Size::ZERO`、rasterize 对空文本返回
//! `ShapedLine::empty(..)` —— 两者都**在 inline box 之前**短路，所以「空文本 + 行内盒」
//! 永远不产生行内盒。可行形状只有「非空文本 + 行内盒」，且文本必须不参与尺寸：
//!
//! - 文本取一个空格（字体里恒有、没有可见像素）；
//! - 节点样式显式写死 `width/height = 图片逻辑尺寸`（taffy 用 definite 尺寸，空格的 advance
//!   不进入布局）；`wrap = NoWrap` 保证空格不会把行内盒挤到下一行；
//! - `line_height = 1.0` 且字号 = 图片高度 ⇒ strut 与图片同高，行高不被字体度量撑大；
//! - `align = Top` ⇒ 图片顶边贴行顶，不受字体基线影响；行内盒 `index = 0` ⇒ 图片 x = 0。
//!
//! 结果（测试锁定）：节点布局盒 = 图片逻辑尺寸，`Primitive::Image.bounds` = 同一盒。
//!
//! **已登记的引擎后续项**（spec-delta §6）：给 `NodeKind` 加图片，或至少让空文本的
//! `measure` / rasterize 先看 `inline_boxes` 是否为空；届时本函数删掉空格与固定尺寸两处绕行。
//!
//! # 局限
//!
//! 图片节点是**文本布局叶子**：不能再挂子节点，也不能用 `Style.inner` 表达 flex 尺寸（尺寸就是
//! 行内盒尺寸）。旋转 / 缩放等节点级变换引擎没有支持，旧 `Icon` 的 `Transformation` 因此不迁移。

use smol_str::SmolStr;
use taffy::style::Dimension;
use ui_core::{
    a11y::A11yOverride,
    app::App,
    error::EngineError,
    math::Size,
    node::{builder::NodeBuilder, id::NodeId},
    paint::{color::Color, style::Style},
    resources::ResourceId,
    text::{InlineAlign, InlineBoxPaint, InlineBoxSpec, TextOverflow, TextStyle, TextWrap},
    window::WindowId,
};

/// 在 `parent` 末尾挂一个「只画一张图」的节点，返回新节点句柄。
///
/// `size` 是**逻辑像素**（图片在布局里的尺寸）；`modulation` 直接进
/// `InlineBoxPaint::Image`，由引擎在 paint 期乘到 texel 上（白色字形 PNG ⇒ 任意颜色）。
///
/// # Errors
/// 窗口已关闭 / 父节点陈旧 / 布局叶子等 `EngineError`（引擎的建树错误面）。
pub(crate) fn mount(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    resource: ResourceId,
    size: Size,
    modulation: Color,
) -> Result<NodeId, EngineError> {
    let spec = InlineBoxSpec {
        // 应用句柄：paint 期用它把 `InlineBoxSpec.paint` 与行内盒落点配对；这里用资源 id 的
        // 单值形式，天然唯一（同一节点只有一个盒）。
        id: resource.to_bits(),
        index: 0,
        size,
        // `None` = 盒底边对齐文本基线；配合 `Top` 对齐后落点是 `[0, h)`（见模块文档）。
        baseline: None,
        align: InlineAlign::Top,
        paint: Some(InlineBoxPaint::Image {
            resource,
            modulation,
        }),
    };
    let mut text_style = TextStyle::new(size.height);
    text_style.line_height = Some(1.0);
    text_style.wrap = TextWrap::NoWrap;
    text_style.overflow = TextOverflow::Clip;
    // 空格没有可见像素；颜色仍跟随调制色，保证「意外可见」时也不会画出与主题矛盾的色块。
    text_style.color = modulation;

    let mut style = Style::default();
    style.inner.size.width = Dimension::length(size.width);
    style.inner.size.height = Dimension::length(size.height);

    let tree = &mut app
        .window_mut(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .tree_mut();
    NodeBuilder::text(SmolStr::new_static(" "), text_style)
        .inline_boxes([spec])
        .style(style)
        // 图片是**装饰性叶子**：占位文本是一个空格，不隐藏的话 a11y 会播报一个空白文本节点。
        // 有语义的图标由使用方在容器上给 role/label（旧 `Icon` 也没有可访问名，行为不变）。
        .a11y(A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        })
        .build(tree, parent)
}
