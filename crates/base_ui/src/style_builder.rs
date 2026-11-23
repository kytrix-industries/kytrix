//! 应用层薄样式构建器（设计 D24 的硬边界）。
//!
//! 引擎**不提供**任何样式构造器（`ui_core::paint::style` 只有三个数据契约 + `Default`），布局属性由
//! 应用直接构造 `taffy::Style<SmolStr>` 再包一层。这个文件就是那一层，按 D24 的边界写得尽可能薄：
//!
//! 1. **只覆盖高频组合**，方法集合固定：[`StyleBuilder`] 35 个方法（含 `new`/`build`），
//!    [`PaintBuilder`] 6 个；不新增"为了以后可能用到"的入口。
//! 2. **无宏、无字符串选择器、无嵌套闭包 DSL、无扩展 trait**：方法就是字段直写。
//! 3. **无 `.when()`/`.map()`/条件样式**：条件样式走 `bind_style` + `Signal`（运行时样式必须走
//!    `StylePatch`，设计 §4.9.2）。
//! 4. **复杂布局（grid、多角不同圆角、自定义 measure、clip 圆角、渐变）直接构造 `Style`/
//!    `PaintSpec`**，不塞进 builder。
//! 5. **builder 只在挂载阶段调用一次，返回值是引擎值**（`Style`/`PaintSpec`），不是可继续 refine
//!    的对象；没有 `Styled`/`StyleRefinement` 形状。
//!
//! # 失效通道（哪些属性影响布局）
//!
//! 引擎把样式分成三条通道（设计 §4.9）：`Style`（布局 + 子树裁剪）走 `LAYOUT`；`PaintSpec`（静态绘制）
//! 走 `PAINT`；`StylePatch`（动态属性）走 `BIND → PAINT`（`affects_layout` 时追加 `LAYOUT`）。
//!
//! - [`StyleBuilder`] 的**每个属性方法都写布局字段**（`taffy::Style::inner`），因此它们只影响布局：
//!   挂载期一次写入，结构变化时重写 `Style` 才标 `LAYOUT` 并向祖先冒泡。
//! - [`StyleBuilder::overflow_hidden`] 是唯一同时写 `Style.clip` 的方法：它让子树被裁剪（`LAYOUT` +
//!   子树 lane 重建），并给 taffy 的 `overflow` 置 `Hidden`——CSS `overflow: hidden` 的另一半语义
//!   （flex/grid 子项的自动最小尺寸变 0）。**只设 `clip` 不产生自动最小尺寸语义，只设 taffy
//!   overflow 不产生引擎裁剪**，两半都要。
//! - [`PaintBuilder`] 只写 `PaintSpec`（静态绘制，`PAINT`）；它**不参与布局**。
//!
//! # 布局边框 vs 绘制边框
//!
//! 引擎把旧实现 `.border_1()` 的两件事拆成两条通道：**占位**与**画线**。
//!
//! - 占位（参与布局）：[`StyleBuilder::border`] 写 `taffy::Style::border`，盒子四周各留出线宽，
//!   内容区被挤进去——这正是旧 `border_widths` 的行为；
//! - 画线（不参与布局）：[`PaintBuilder::border`] 写 `PaintSpec.border`，只画内侧边框。
//!
//! 从旧控件迁移时**成对书写**：`StyleBuilder::new().border(1.0)` +
//! `PaintBuilder::new().border(1.0, color)`；只写后者会静默丢掉旧边框的布局占位（审查 F2-2）。
//! `ComponentTokens::border_width` 是这两处共用的值来源。
//!
//! - 动态属性（`background`/`foreground`/`border`/`opacity`/`visible`/`interactive`/`cursor`）**不在
//!   任何 builder 里**：它们属于 `StylePatch`，由 `bind_style` 闭包每帧求值；尺寸会变时作者在
//!   `StylePatch::affects_layout` 置 `true`（builder 无从得知运行期值是否会变）。

use taffy::{
    geometry::{Point, Rect, Size as TaffySize},
    style::{
        AlignItems, Dimension, FlexDirection, JustifyContent, LengthPercentage,
        LengthPercentageAuto, Overflow, Position,
    },
};
use ui_core::paint::{
    color::Color,
    primitive::Border,
    style::{Clip, PaintSpec, ShadowSpec, Shadows, Style},
};

/// 薄布局样式构建器：把高频布局组合写进引擎 [`Style`] 的 `inner`（taffy）与 `clip`。
///
/// 值语义：`build()` 之后不可再改（设计 D24 第 5 条）；`build()` 是唯一出口。
#[derive(Debug, Clone)]
pub struct StyleBuilder {
    style: Style,
}

impl StyleBuilder {
    /// 从默认样式开始（`Style::default()`：flex 容器、row 方向、`align_items: stretch`）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            style: Style::default(),
        }
    }

    /// 主轴横向（CSS `display:flex; flex-direction:row`）。
    #[must_use]
    pub fn flex_row(mut self) -> Self {
        self.style.inner.flex_direction = FlexDirection::Row;
        self
    }

    /// 主轴纵向（CSS `display:flex; flex-direction:column`）。
    #[must_use]
    pub fn flex_col(mut self) -> Self {
        self.style.inner.flex_direction = FlexDirection::Column;
        self
    }

    /// 一个可伸缩的 flex 子项（`flex: 1 1 0`）。
    ///
    /// `flex_shrink = 1.0` 恰好是 taffy 的默认值；这里显式写出来是意图声明（可伸缩、可收缩），
    /// 删掉这一行与保留它布局等价——变异自检里它是**等价变异**，不是测试覆盖缺口。
    #[must_use]
    pub fn flex_1(mut self) -> Self {
        self.style.inner.flex_grow = 1.0;
        self.style.inner.flex_shrink = 1.0;
        self.style.inner.flex_basis = Dimension::length(0.0);
        self
    }

    /// 不伸缩的 flex 子项，按内容尺寸（`flex: 0 0 auto`）。
    #[must_use]
    pub fn flex_none(mut self) -> Self {
        self.style.inner.flex_grow = 0.0;
        self.style.inner.flex_shrink = 0.0;
        self.style.inner.flex_basis = Dimension::auto();
        self
    }

    /// 交叉轴居中。
    #[must_use]
    pub fn items_center(mut self) -> Self {
        self.style.inner.align_items = AlignItems::CENTER;
        self
    }

    /// 主轴居中。
    #[must_use]
    pub fn justify_center(mut self) -> Self {
        self.style.inner.justify_content = JustifyContent::CENTER;
        self
    }

    /// 主轴两端对齐（首尾贴边，中间均分）。
    #[must_use]
    pub fn justify_between(mut self) -> Self {
        self.style.inner.justify_content = JustifyContent::SPACE_BETWEEN;
        self
    }

    /// 主轴靠尾对齐。
    #[must_use]
    pub fn justify_end(mut self) -> Self {
        self.style.inner.justify_content = JustifyContent::END;
        self
    }

    /// 子项间隙（横纵同值）。
    #[must_use]
    pub fn gap(mut self, px: f32) -> Self {
        let gap = LengthPercentage::length(px);
        self.style.inner.gap = TaffySize {
            width: gap,
            height: gap,
        };
        self
    }

    /// 四边内边距。
    #[must_use]
    pub fn padding(mut self, px: f32) -> Self {
        let value = LengthPercentage::length(px);
        self.style.inner.padding = Rect {
            left: value,
            right: value,
            top: value,
            bottom: value,
        };
        self
    }

    /// 左右内边距。
    #[must_use]
    pub fn padding_x(mut self, px: f32) -> Self {
        let value = LengthPercentage::length(px);
        self.style.inner.padding.left = value;
        self.style.inner.padding.right = value;
        self
    }

    /// 上下内边距。
    #[must_use]
    pub fn padding_y(mut self, px: f32) -> Self {
        let value = LengthPercentage::length(px);
        self.style.inner.padding.top = value;
        self.style.inner.padding.bottom = value;
        self
    }

    /// 四向各自的内边距（上、右、下、左，CSS 顺序）。
    #[must_use]
    pub fn padding_sides(mut self, top: f32, right: f32, bottom: f32, left: f32) -> Self {
        self.style.inner.padding = Rect {
            left: LengthPercentage::length(left),
            right: LengthPercentage::length(right),
            top: LengthPercentage::length(top),
            bottom: LengthPercentage::length(bottom),
        };
        self
    }

    /// 四边外边距。
    #[must_use]
    pub fn margin(mut self, px: f32) -> Self {
        let value = LengthPercentageAuto::length(px);
        self.style.inner.margin = Rect {
            left: value,
            right: value,
            top: value,
            bottom: value,
        };
        self
    }

    /// 四向各自的外边距（上、右、下、左，CSS 顺序）。
    #[must_use]
    pub fn margin_sides(mut self, top: f32, right: f32, bottom: f32, left: f32) -> Self {
        self.style.inner.margin = Rect {
            left: LengthPercentageAuto::length(left),
            right: LengthPercentageAuto::length(right),
            top: LengthPercentageAuto::length(top),
            bottom: LengthPercentageAuto::length(bottom),
        };
        self
    }

    /// **布局**边框：taffy 在盒子四周各留出 `width`（CSS `border-width`），内容区被挤进去。
    ///
    /// 它只占位、**不画任何像素**。要看得见的边缘，另用 [`PaintBuilder::border`] 写绘制宽度与
    /// 颜色：旧实现 `.border_1()` 同时做这两件事，迁移时必须成对写（模块文档「布局边框 vs
    /// 绘制边框」）。动态边框（按下变宽/变色）属于 `StylePatch`，不在 builder 里。
    #[must_use]
    pub fn border(mut self, width: f32) -> Self {
        let value = LengthPercentage::length(width);
        self.style.inner.border = Rect {
            left: value,
            right: value,
            top: value,
            bottom: value,
        };
        self
    }

    /// 宽度（逻辑像素）。
    #[must_use]
    pub fn width(mut self, px: f32) -> Self {
        self.style.inner.size.width = Dimension::length(px);
        self
    }

    /// 高度（逻辑像素）。
    #[must_use]
    pub fn height(mut self, px: f32) -> Self {
        self.style.inner.size.height = Dimension::length(px);
        self
    }

    /// 宽高（逻辑像素）。
    #[must_use]
    pub fn size(mut self, width: f32, height: f32) -> Self {
        self.style.inner.size = TaffySize {
            width: Dimension::length(width),
            height: Dimension::length(height),
        };
        self
    }

    /// 宽度撑满父容器（`100%`）。
    #[must_use]
    pub fn width_full(mut self) -> Self {
        self.style.inner.size.width = Dimension::percent(1.0);
        self
    }

    /// 高度撑满父容器（`100%`）。
    #[must_use]
    pub fn height_full(mut self) -> Self {
        self.style.inner.size.height = Dimension::percent(1.0);
        self
    }

    /// 宽高都撑满父容器（`size_full`，旧代码里最高频的组合，78 处）。
    #[must_use]
    pub fn size_full(mut self) -> Self {
        self.style.inner.size = TaffySize {
            width: Dimension::percent(1.0),
            height: Dimension::percent(1.0),
        };
        self
    }

    /// 最小宽度 0（让 flex/grid 子项可以被压缩到内容以下，旧代码 34 处）。
    #[must_use]
    pub fn min_width_0(mut self) -> Self {
        self.style.inner.min_size.width = LengthPercentageAuto::length(0.0);
        self
    }

    /// 最小高度 0。
    #[must_use]
    pub fn min_height_0(mut self) -> Self {
        self.style.inner.min_size.height = LengthPercentageAuto::length(0.0);
        self
    }

    /// 最大宽度（逻辑像素）。
    #[must_use]
    pub fn max_width(mut self, px: f32) -> Self {
        self.style.inner.max_size.width = LengthPercentageAuto::length(px);
        self
    }

    /// 最大高度（逻辑像素）。
    #[must_use]
    pub fn max_height(mut self, px: f32) -> Self {
        self.style.inner.max_size.height = LengthPercentageAuto::length(px);
        self
    }

    /// 裁剪子树（CSS `overflow: hidden`）：`Style.clip = Clip::Bounds` + taffy `overflow: hidden`。
    ///
    /// 引擎的裁剪只认 [`Clip::Bounds`]（按 padding box 裁子树，命中测试的 clip 链同源）；taffy 的
    /// `overflow` 决定 flex/grid 子项的自动最小尺寸是否为 0。两半都是这个 CSS 语义的组成部分。
    /// **滚动容器**还要在挂载后调用 `App::make_scroll`（引擎要求 `Clip::Bounds`）。
    #[must_use]
    pub fn overflow_hidden(mut self) -> Self {
        self.style.clip = Clip::Bounds;
        self.style.inner.overflow = Point {
            x: Overflow::Hidden,
            y: Overflow::Hidden,
        };
        self
    }

    /// 相对定位（仍占流内空间，`inset` 相对自身位置偏移）。
    #[must_use]
    pub fn relative(mut self) -> Self {
        self.style.inner.position = Position::Relative;
        self
    }

    /// 绝对定位（脱离正常流，`inset` 相对最近定位祖先）。
    #[must_use]
    pub fn absolute(mut self) -> Self {
        self.style.inner.position = Position::Absolute;
        self
    }

    /// 绝对/相对定位的上边距偏移（逻辑像素）。
    #[must_use]
    pub fn top(mut self, px: f32) -> Self {
        self.style.inner.inset.top = LengthPercentageAuto::length(px);
        self
    }

    /// 右边距偏移（逻辑像素）。
    #[must_use]
    pub fn right(mut self, px: f32) -> Self {
        self.style.inner.inset.right = LengthPercentageAuto::length(px);
        self
    }

    /// 下边距偏移（逻辑像素）。
    #[must_use]
    pub fn bottom(mut self, px: f32) -> Self {
        self.style.inner.inset.bottom = LengthPercentageAuto::length(px);
        self
    }

    /// 左边距偏移（逻辑像素）。
    #[must_use]
    pub fn left(mut self, px: f32) -> Self {
        self.style.inner.inset.left = LengthPercentageAuto::length(px);
        self
    }

    /// 唯一出口：挂载期交给 `NodeBuilder::style` 的 [`Style`] 值。
    #[must_use]
    pub fn build(self) -> Style {
        self.style
    }
}

impl Default for StyleBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// 薄静态绘制构建器：高频的 `PaintSpec` 组合（背景 / 圆角 / 边框 / 阴影）。
///
/// 引擎的 `PaintSpec` 只有 `Default`（`paint/style.rs`），静态绘制因此需要一个写 5 个字段的地方；
/// 这里只包高频项，复杂绘制（渐变、多角不同圆角）直接构造 `PaintSpec`（D24 第 4 条）。动态覆盖
/// （hover 背景等）走 `bind_style` 的 `StylePatch.background`，不在这里。
#[derive(Debug, Clone)]
pub struct PaintBuilder {
    paint: PaintSpec,
}

impl PaintBuilder {
    /// 从"不画任何东西"开始（透明背景、无边框、无圆角）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            paint: PaintSpec::default(),
        }
    }

    /// 背景填充色（`Color::TRANSPARENT` = 不画背景）。
    #[must_use]
    pub fn background(mut self, color: Color) -> Self {
        self.paint.background = color;
        self
    }

    /// 四角同值圆角（逻辑像素；编码期 clamp 到 `min(w, h) / 2`）。
    #[must_use]
    pub fn rounded(mut self, px: f32) -> Self {
        self.paint.corner_radius = [px; 4];
        self
    }

    /// 内侧边框：绘制宽度 + 颜色（绘制宽度**不参与布局**）。
    ///
    /// 这是**画线**通道；旧 `.border_1()` 的布局占位由 [`StyleBuilder::border`] 写
    /// `Style.inner.border`，两者成对使用（模块文档「布局边框 vs 绘制边框」）。
    #[must_use]
    pub fn border(mut self, width: f32, color: Color) -> Self {
        self.paint.border = Some(Border { width, color });
        self
    }

    /// 解析阴影（`rect` 由布局盒给出，不进样式）。单层入口；多层阴影直接写
    /// `PaintSpec.shadows`（E13 的 `Shadows`）。
    #[must_use]
    pub fn shadow(mut self, shadow: ShadowSpec) -> Self {
        self.paint.shadows = Shadows::from(shadow);
        self
    }

    /// 唯一出口：挂载期交给 `NodeBuilder::paint` 的 [`PaintSpec`] 值。
    #[must_use]
    pub fn build(self) -> PaintSpec {
        self.paint
    }
}

impl Default for PaintBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;
    use taffy::{
        geometry::{Point, Rect, Size as TaffySize},
        style::{
            AlignItems, Dimension, FlexDirection, JustifyContent, LengthPercentage,
            LengthPercentageAuto, Overflow, Position,
        },
    };
    use ui_core::paint::{
        color::Color,
        primitive::Border,
        style::{Clip, PaintSpec, ShadowSpec, Shadows, Style},
    };

    use super::{PaintBuilder, StyleBuilder};

    /// 直接构造 taffy 样式的测试助手（逐字段断言用；不是生产 API）。
    fn taffy_style() -> taffy::Style<SmolStr> {
        taffy::Style::<SmolStr>::DEFAULT
    }

    /// `StyleBuilder::new().build()` 必须就是 `Style::default()`（不多写一个字段）。
    #[test]
    fn an_empty_builder_builds_the_default_style() {
        assert_eq!(StyleBuilder::new().build(), Style::default());
        assert_eq!(StyleBuilder::default().build(), Style::default());
    }

    /// 抽样 ≥15 组组合：builder 输出与直接构造 `taffy::Style` 逐字段等价。
    #[test]
    fn style_builder_matches_direct_taffy_style() {
        let cases: [(&str, fn(StyleBuilder) -> Style, Style); 21] = [
            (
                "flex_row",
                |builder| builder.flex_row().build(),
                Style {
                    inner: taffy::Style {
                        flex_direction: FlexDirection::Row,
                        ..taffy_style()
                    },
                    clip: Clip::None,
                },
            ),
            (
                "flex_col",
                |builder| builder.flex_col().build(),
                Style {
                    inner: taffy::Style {
                        flex_direction: FlexDirection::Column,
                        ..taffy_style()
                    },
                    clip: Clip::None,
                },
            ),
            (
                "flex_1",
                |builder| builder.flex_1().build(),
                Style {
                    inner: taffy::Style {
                        flex_grow: 1.0,
                        flex_shrink: 1.0,
                        flex_basis: Dimension::length(0.0),
                        ..taffy_style()
                    },
                    clip: Clip::None,
                },
            ),
            (
                "flex_none",
                |builder| builder.flex_none().build(),
                Style {
                    inner: taffy::Style {
                        flex_grow: 0.0,
                        flex_shrink: 0.0,
                        flex_basis: Dimension::auto(),
                        ..taffy_style()
                    },
                    clip: Clip::None,
                },
            ),
            (
                "items_center",
                |builder| builder.items_center().build(),
                Style {
                    inner: taffy::Style {
                        align_items: AlignItems::CENTER,
                        ..taffy_style()
                    },
                    clip: Clip::None,
                },
            ),
            (
                "justify_center",
                |builder| builder.justify_center().build(),
                Style {
                    inner: taffy::Style {
                        justify_content: JustifyContent::CENTER,
                        ..taffy_style()
                    },
                    clip: Clip::None,
                },
            ),
            (
                "justify_between",
                |builder| builder.justify_between().build(),
                Style {
                    inner: taffy::Style {
                        justify_content: JustifyContent::SPACE_BETWEEN,
                        ..taffy_style()
                    },
                    clip: Clip::None,
                },
            ),
            (
                "justify_end",
                |builder| builder.justify_end().build(),
                Style {
                    inner: taffy::Style {
                        justify_content: JustifyContent::END,
                        ..taffy_style()
                    },
                    clip: Clip::None,
                },
            ),
            (
                "gap",
                |builder| builder.gap(8.0).build(),
                Style {
                    inner: taffy::Style {
                        gap: TaffySize {
                            width: LengthPercentage::length(8.0),
                            height: LengthPercentage::length(8.0),
                        },
                        ..taffy_style()
                    },
                    clip: Clip::None,
                },
            ),
            (
                "padding",
                |builder| builder.padding(16.0).build(),
                Style {
                    inner: taffy::Style {
                        padding: Rect {
                            left: LengthPercentage::length(16.0),
                            right: LengthPercentage::length(16.0),
                            top: LengthPercentage::length(16.0),
                            bottom: LengthPercentage::length(16.0),
                        },
                        ..taffy_style()
                    },
                    clip: Clip::None,
                },
            ),
            (
                "padding_x_y",
                |builder| builder.padding_x(12.0).padding_y(8.0).build(),
                Style {
                    inner: taffy::Style {
                        padding: Rect {
                            left: LengthPercentage::length(12.0),
                            right: LengthPercentage::length(12.0),
                            top: LengthPercentage::length(8.0),
                            bottom: LengthPercentage::length(8.0),
                        },
                        ..taffy_style()
                    },
                    clip: Clip::None,
                },
            ),
            (
                "padding_sides",
                |builder| builder.padding_sides(1.0, 2.0, 3.0, 4.0).build(),
                Style {
                    inner: taffy::Style {
                        padding: Rect {
                            left: LengthPercentage::length(4.0),
                            right: LengthPercentage::length(2.0),
                            top: LengthPercentage::length(1.0),
                            bottom: LengthPercentage::length(3.0),
                        },
                        ..taffy_style()
                    },
                    clip: Clip::None,
                },
            ),
            (
                "margin",
                |builder| builder.margin(5.0).build(),
                Style {
                    inner: taffy::Style {
                        margin: Rect {
                            left: LengthPercentageAuto::length(5.0),
                            right: LengthPercentageAuto::length(5.0),
                            top: LengthPercentageAuto::length(5.0),
                            bottom: LengthPercentageAuto::length(5.0),
                        },
                        ..taffy_style()
                    },
                    clip: Clip::None,
                },
            ),
            (
                "margin_sides",
                |builder| builder.margin_sides(1.0, 2.0, 3.0, 4.0).build(),
                Style {
                    inner: taffy::Style {
                        margin: Rect {
                            left: LengthPercentageAuto::length(4.0),
                            right: LengthPercentageAuto::length(2.0),
                            top: LengthPercentageAuto::length(1.0),
                            bottom: LengthPercentageAuto::length(3.0),
                        },
                        ..taffy_style()
                    },
                    clip: Clip::None,
                },
            ),
            (
                "layout_border",
                |builder| builder.border(1.0).build(),
                Style {
                    inner: taffy::Style {
                        border: Rect {
                            left: LengthPercentage::length(1.0),
                            right: LengthPercentage::length(1.0),
                            top: LengthPercentage::length(1.0),
                            bottom: LengthPercentage::length(1.0),
                        },
                        ..taffy_style()
                    },
                    clip: Clip::None,
                },
            ),
            (
                "width_height",
                |builder| builder.width(120.0).height(40.0).build(),
                Style {
                    inner: taffy::Style {
                        size: TaffySize {
                            width: Dimension::length(120.0),
                            height: Dimension::length(40.0),
                        },
                        ..taffy_style()
                    },
                    clip: Clip::None,
                },
            ),
            (
                "size",
                |builder| builder.size(120.0, 40.0).build(),
                Style {
                    inner: taffy::Style {
                        size: TaffySize {
                            width: Dimension::length(120.0),
                            height: Dimension::length(40.0),
                        },
                        ..taffy_style()
                    },
                    clip: Clip::None,
                },
            ),
            (
                "width_full_size_full",
                |builder| builder.width_full().height_full().build(),
                Style {
                    inner: taffy::Style {
                        size: TaffySize {
                            width: Dimension::percent(1.0),
                            height: Dimension::percent(1.0),
                        },
                        ..taffy_style()
                    },
                    clip: Clip::None,
                },
            ),
            (
                "size_full",
                |builder| builder.size_full().build(),
                Style {
                    inner: taffy::Style {
                        size: TaffySize {
                            width: Dimension::percent(1.0),
                            height: Dimension::percent(1.0),
                        },
                        ..taffy_style()
                    },
                    clip: Clip::None,
                },
            ),
            (
                "min_max",
                |builder| {
                    builder
                        .min_width_0()
                        .min_height_0()
                        .max_width(640.0)
                        .max_height(480.0)
                        .build()
                },
                Style {
                    inner: taffy::Style {
                        min_size: TaffySize {
                            width: LengthPercentageAuto::length(0.0),
                            height: LengthPercentageAuto::length(0.0),
                        },
                        max_size: TaffySize {
                            width: LengthPercentageAuto::length(640.0),
                            height: LengthPercentageAuto::length(480.0),
                        },
                        ..taffy_style()
                    },
                    clip: Clip::None,
                },
            ),
            (
                "overflow_and_position",
                |builder| {
                    builder
                        .overflow_hidden()
                        .relative()
                        .top(1.0)
                        .right(2.0)
                        .bottom(3.0)
                        .left(4.0)
                        .build()
                },
                Style {
                    inner: taffy::Style {
                        overflow: Point {
                            x: Overflow::Hidden,
                            y: Overflow::Hidden,
                        },
                        position: Position::Relative,
                        inset: Rect {
                            left: LengthPercentageAuto::length(4.0),
                            right: LengthPercentageAuto::length(2.0),
                            top: LengthPercentageAuto::length(1.0),
                            bottom: LengthPercentageAuto::length(3.0),
                        },
                        ..taffy_style()
                    },
                    clip: Clip::Bounds,
                },
            ),
        ];

        assert!(cases.len() >= 15, "抽样组合数必须 ≥15");
        for (name, build, expected) in cases {
            assert_eq!(build(StyleBuilder::new()), expected, "组合 {name}");
        }
    }

    /// 逐字段断言：每个方法只动它自己的字段（其余保持默认），且默认值不被意外改写。
    #[test]
    fn each_method_writes_only_its_own_fields() {
        let default = taffy_style();

        let column = StyleBuilder::new().flex_col().build();
        assert_eq!(column.inner.flex_direction, FlexDirection::Column);
        assert_eq!(column.inner.align_items, default.align_items);
        assert_eq!(column.inner.justify_content, default.justify_content);
        assert_eq!(column.inner.gap, default.gap);

        let centered = StyleBuilder::new().flex_row().items_center().build();
        assert_eq!(centered.inner.flex_direction, FlexDirection::Row);
        assert_eq!(centered.inner.align_items, AlignItems::CENTER);
        assert_eq!(centered.inner.justify_content, default.justify_content);

        let insets = StyleBuilder::new().relative().top(4.0).left(8.0).build();
        assert_eq!(insets.inner.position, Position::Relative);
        assert_eq!(insets.inner.inset.top, LengthPercentageAuto::length(4.0));
        assert_eq!(insets.inner.inset.left, LengthPercentageAuto::length(8.0));
        assert_eq!(insets.inner.inset.right, LengthPercentageAuto::auto());
        assert_eq!(insets.inner.inset.bottom, LengthPercentageAuto::auto());

        let absolute = StyleBuilder::new().absolute().build();
        assert_eq!(absolute.inner.position, Position::Absolute);
        assert_eq!(absolute.clip, Clip::None);
        assert_eq!(absolute.inner.overflow, default.overflow);
    }

    /// `overflow_hidden` 同时写 `Clip::Bounds` 与 taffy overflow——两半都是 CSS 语义的组成部分。
    #[test]
    fn overflow_hidden_sets_both_the_engine_clip_and_the_taffy_overflow() {
        let style = StyleBuilder::new().overflow_hidden().build();

        assert_eq!(style.clip, Clip::Bounds);
        assert_eq!(
            style.inner.overflow,
            Point {
                x: Overflow::Hidden,
                y: Overflow::Hidden,
            }
        );
        // 除 clip/overflow 外没有别的字段被改。
        assert_eq!(style.inner.size, taffy_style().size);
        assert_eq!(style.inner.padding, taffy_style().padding);
    }

    /// 布局边框占位、绘制边框画线：引擎把旧 `.border_1()` 拆成两条通道，迁移时必须成对写。
    ///
    /// 审查 F2-2：只写 `PaintBuilder::border` 会静默丢掉旧边框的布局占位；`StyleBuilder::border`
    /// 是补上的那条入口（写 `taffy::Style::border`，不画像素）。
    #[test]
    fn a_layout_border_takes_space_while_the_paint_border_draws() {
        use crate::theme::components::ComponentTokens;

        let style = StyleBuilder::new().border(1.0).build();
        for side in [
            style.inner.border.left,
            style.inner.border.right,
            style.inner.border.top,
            style.inner.border.bottom,
        ] {
            assert_eq!(side, LengthPercentage::length(1.0), "四边都占位");
        }
        assert_eq!(style.clip, Clip::None);

        // 只写布局边框：绘制通道仍然是"不画"。
        assert!(
            PaintBuilder::new().build().border.is_none(),
            "布局边框不画线（画线是 PaintBuilder 的职责）"
        );

        // 成对写法：同一个 token 线宽同时进两条通道。
        let width = ComponentTokens::standard().border_width;
        let style = StyleBuilder::new().border(width).build();
        let paint = PaintBuilder::new().border(width, Color::WHITE).build();
        assert_eq!(style.inner.border.top, LengthPercentage::length(width));
        assert_eq!(
            paint.border,
            Some(Border {
                width,
                color: Color::WHITE,
            })
        );
    }

    /// 链式组合只写它点到的字段：一个典型面板（列 + 撑满 + 间隙 + 内边距 + 居中）。
    #[test]
    fn a_typical_panel_chain_matches_the_direct_style() {
        let built = StyleBuilder::new()
            .flex_col()
            .size_full()
            .gap(8.0)
            .padding(16.0)
            .items_center()
            .build();
        let expected = Style {
            inner: taffy::Style {
                flex_direction: FlexDirection::Column,
                size: TaffySize {
                    width: Dimension::percent(1.0),
                    height: Dimension::percent(1.0),
                },
                gap: TaffySize {
                    width: LengthPercentage::length(8.0),
                    height: LengthPercentage::length(8.0),
                },
                padding: Rect {
                    left: LengthPercentage::length(16.0),
                    right: LengthPercentage::length(16.0),
                    top: LengthPercentage::length(16.0),
                    bottom: LengthPercentage::length(16.0),
                },
                align_items: AlignItems::CENTER,
                ..taffy_style()
            },
            clip: Clip::None,
        };

        assert_eq!(built, expected);
    }

    /// `PaintBuilder` 与直接构造 `PaintSpec` 等价（背景 / 四角圆角 / 边框 / 阴影）。
    #[test]
    fn paint_builder_matches_the_direct_paint_spec() {
        let shadow = ShadowSpec {
            offset: ui_core::math::Point::new(0.0, 2.0),
            blur: 8.0,
            spread: 0.0,
            color: Color::srgb(0.0, 0.0, 0.0, 0.25),
        };
        let built = PaintBuilder::new()
            .background(Color::srgb(1.0, 0.0, 0.0, 1.0))
            .rounded(6.0)
            .border(1.0, Color::WHITE)
            .shadow(shadow)
            .build();
        let expected = PaintSpec {
            background: Color::srgb(1.0, 0.0, 0.0, 1.0),
            border: Some(Border {
                width: 1.0,
                color: Color::WHITE,
            }),
            corner_radius: [6.0; 4],
            shadows: Shadows::from(shadow),
            gradient: None,
        };

        assert_eq!(built, expected);
        assert_eq!(PaintBuilder::default().build(), PaintSpec::default());
    }

    /// 圆角四角同值；`border` 的宽度是绘制宽度（不进布局）。
    #[test]
    fn paint_builder_rounded_and_border_are_draw_only() {
        let paint = PaintBuilder::new()
            .rounded(4.0)
            .border(2.0, Color::WHITE)
            .build();

        assert_eq!(paint.corner_radius, [4.0; 4]);
        assert_eq!(
            paint.border,
            Some(Border {
                width: 2.0,
                color: Color::WHITE,
            })
        );
        assert_eq!(paint.background, Color::TRANSPARENT);
        assert!(paint.shadows.is_empty());
        assert!(paint.gradient.is_none());
    }
}
