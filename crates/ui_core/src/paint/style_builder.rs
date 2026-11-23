use taffy::style::{
    AlignItems, Dimension, FlexDirection, JustifyContent, LengthPercentageAuto, Overflow, Position,
};

use crate::{
    math::LogicalPx,
    paint::{
        color::Color,
        primitive::Border,
        style::{Clip, PaintSpec, Style},
    },
};

#[derive(Debug, Clone)]
pub struct StyleBuilder {
    style: Style,
}

impl StyleBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            style: Style::default(),
        }
    }

    #[must_use]
    pub fn flex_row(mut self) -> Self {
        self.style.flex_direction(FlexDirection::Row);
        self
    }

    #[must_use]
    pub fn flex_col(mut self) -> Self {
        self.style.flex_direction(FlexDirection::Column);
        self
    }

    #[must_use]
    pub fn flex_1(mut self) -> Self {
        self.style.flex(1.0, 1.0, Dimension::length(0.0));
        self
    }

    #[must_use]
    pub fn flex_none(mut self) -> Self {
        self.style.flex(0.0, 0.0, Dimension::auto());
        self
    }

    #[must_use]
    pub fn items_center(mut self) -> Self {
        self.style.align_items(AlignItems::CENTER);
        self
    }

    #[must_use]
    pub fn justify_center(mut self) -> Self {
        self.style.justify_content(JustifyContent::CENTER);
        self
    }

    #[must_use]
    pub fn justify_between(mut self) -> Self {
        self.style.justify_content(JustifyContent::SPACE_BETWEEN);
        self
    }

    #[must_use]
    pub fn justify_end(mut self) -> Self {
        self.style.justify_content(JustifyContent::END);
        self
    }

    #[must_use]
    pub fn gap(mut self, px: LogicalPx) -> Self {
        self.style.gap(px);
        self
    }

    #[must_use]
    pub fn padding(mut self, px: LogicalPx) -> Self {
        self.style.padding(px);
        self
    }

    #[must_use]
    pub fn padding_x(mut self, px: LogicalPx) -> Self {
        self.style.padding_x(px);
        self
    }

    #[must_use]
    pub fn padding_y(mut self, px: LogicalPx) -> Self {
        self.style.padding_y(px);
        self
    }

    #[must_use]
    pub fn padding_sides(
        mut self,
        top: LogicalPx,
        right: LogicalPx,
        bottom: LogicalPx,
        left: LogicalPx,
    ) -> Self {
        self.style.padding_sides(top, right, bottom, left);
        self
    }

    #[must_use]
    pub fn margin_sides(
        mut self,
        top: LogicalPx,
        right: LogicalPx,
        bottom: LogicalPx,
        left: LogicalPx,
    ) -> Self {
        self.style.margin_sides(top, right, bottom, left);
        self
    }

    #[must_use]
    pub fn border(mut self, width: LogicalPx) -> Self {
        self.style.border(width);
        self
    }

    #[must_use]
    pub fn width(mut self, px: LogicalPx) -> Self {
        self.style.width(Dimension::length(px.value()));
        self
    }

    #[must_use]
    pub fn height(mut self, px: LogicalPx) -> Self {
        self.style.height(Dimension::length(px.value()));
        self
    }

    #[must_use]
    pub fn size(mut self, width: LogicalPx, height: LogicalPx) -> Self {
        self.style.size_length(width, height);
        self
    }

    #[must_use]
    pub fn width_full(mut self) -> Self {
        self.style.width(Dimension::percent(1.0));
        self
    }

    #[must_use]
    pub fn height_full(mut self) -> Self {
        self.style.height(Dimension::percent(1.0));
        self
    }

    #[must_use]
    pub fn size_full(mut self) -> Self {
        self.style.size_percent(1.0, 1.0);
        self
    }

    #[must_use]
    pub fn min_width_0(mut self) -> Self {
        self.style.min_width(LengthPercentageAuto::length(0.0));
        self
    }

    #[must_use]
    pub fn min_height_0(mut self) -> Self {
        self.style.min_height(LengthPercentageAuto::length(0.0));
        self
    }

    #[must_use]
    pub fn max_width(mut self, px: LogicalPx) -> Self {
        self.style
            .max_width(LengthPercentageAuto::length(px.value()));
        self
    }

    #[must_use]
    pub fn max_height(mut self, px: LogicalPx) -> Self {
        self.style
            .max_height(LengthPercentageAuto::length(px.value()));
        self
    }

    #[must_use]
    pub fn overflow_hidden(mut self) -> Self {
        self.style.overflow(Overflow::Hidden);
        self.style.clip = Clip::Bounds;
        self
    }

    #[must_use]
    pub fn relative(mut self) -> Self {
        self.style.position(Position::Relative);
        self
    }

    #[must_use]
    pub fn absolute(mut self) -> Self {
        self.style.position(Position::Absolute);
        self
    }

    #[must_use]
    pub fn top(mut self, px: LogicalPx) -> Self {
        self.style.inset_top(px);
        self
    }

    #[must_use]
    pub fn right(mut self, px: LogicalPx) -> Self {
        self.style.inset_right(px);
        self
    }

    #[must_use]
    pub fn bottom(mut self, px: LogicalPx) -> Self {
        self.style.inset_bottom(px);
        self
    }

    #[must_use]
    pub fn left(mut self, px: LogicalPx) -> Self {
        self.style.inset_left(px);
        self
    }

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

#[derive(Debug, Clone)]
pub struct PaintBuilder {
    paint: PaintSpec,
}

impl PaintBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            paint: PaintSpec::default(),
        }
    }

    #[must_use]
    pub fn background(mut self, color: Color) -> Self {
        self.paint.background = color;
        self
    }

    #[must_use]
    pub fn rounded(mut self, px: LogicalPx) -> Self {
        self.paint.corner_radius = [px; 4];
        self
    }

    #[must_use]
    pub fn border(mut self, width: LogicalPx, color: Color) -> Self {
        self.paint.border = Some(Border { width, color });
        self
    }

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

    use super::{PaintBuilder, StyleBuilder};
    use crate::{
        math::px,
        paint::{
            color::Color,
            primitive::Border,
            style::{Clip, PaintSpec, Shadows, Style},
        },
    };

    fn taffy_style() -> taffy::Style<SmolStr> {
        taffy::Style::<SmolStr>::DEFAULT
    }

    #[test]
    fn an_empty_builder_builds_the_default_style() {
        assert_eq!(StyleBuilder::new().build(), Style::default());
        assert_eq!(StyleBuilder::default().build(), Style::default());
    }

    #[test]
    fn style_builder_matches_direct_taffy_style() {
        let cases: [(&str, fn(StyleBuilder) -> Style, taffy::Style<SmolStr>, Clip); 20] = [
            (
                "flex_row",
                |builder| builder.flex_row().build(),
                taffy::Style {
                    flex_direction: FlexDirection::Row,
                    ..taffy_style()
                },
                Clip::None,
            ),
            (
                "flex_col",
                |builder| builder.flex_col().build(),
                taffy::Style {
                    flex_direction: FlexDirection::Column,
                    ..taffy_style()
                },
                Clip::None,
            ),
            (
                "flex_1",
                |builder| builder.flex_1().build(),
                taffy::Style {
                    flex_grow: 1.0,
                    flex_shrink: 1.0,
                    flex_basis: Dimension::length(0.0),
                    ..taffy_style()
                },
                Clip::None,
            ),
            (
                "flex_none",
                |builder| builder.flex_none().build(),
                taffy::Style {
                    flex_grow: 0.0,
                    flex_shrink: 0.0,
                    flex_basis: Dimension::auto(),
                    ..taffy_style()
                },
                Clip::None,
            ),
            (
                "items_center",
                |builder| builder.items_center().build(),
                taffy::Style {
                    align_items: AlignItems::CENTER,
                    ..taffy_style()
                },
                Clip::None,
            ),
            (
                "justify_center",
                |builder| builder.justify_center().build(),
                taffy::Style {
                    justify_content: JustifyContent::CENTER,
                    ..taffy_style()
                },
                Clip::None,
            ),
            (
                "justify_between",
                |builder| builder.justify_between().build(),
                taffy::Style {
                    justify_content: JustifyContent::SPACE_BETWEEN,
                    ..taffy_style()
                },
                Clip::None,
            ),
            (
                "justify_end",
                |builder| builder.justify_end().build(),
                taffy::Style {
                    justify_content: JustifyContent::END,
                    ..taffy_style()
                },
                Clip::None,
            ),
            (
                "gap",
                |builder| builder.gap(px(8.0)).build(),
                taffy::Style {
                    gap: TaffySize {
                        width: LengthPercentage::length(8.0),
                        height: LengthPercentage::length(8.0),
                    },
                    ..taffy_style()
                },
                Clip::None,
            ),
            (
                "padding",
                |builder| builder.padding(px(16.0)).build(),
                taffy::Style {
                    padding: Rect {
                        left: LengthPercentage::length(16.0),
                        right: LengthPercentage::length(16.0),
                        top: LengthPercentage::length(16.0),
                        bottom: LengthPercentage::length(16.0),
                    },
                    ..taffy_style()
                },
                Clip::None,
            ),
            (
                "padding_x_y",
                |builder| builder.padding_x(px(12.0)).padding_y(px(8.0)).build(),
                taffy::Style {
                    padding: Rect {
                        left: LengthPercentage::length(12.0),
                        right: LengthPercentage::length(12.0),
                        top: LengthPercentage::length(8.0),
                        bottom: LengthPercentage::length(8.0),
                    },
                    ..taffy_style()
                },
                Clip::None,
            ),
            (
                "padding_sides",
                |builder| {
                    builder
                        .padding_sides(px(1.0), px(2.0), px(3.0), px(4.0))
                        .build()
                },
                taffy::Style {
                    padding: Rect {
                        left: LengthPercentage::length(4.0),
                        right: LengthPercentage::length(2.0),
                        top: LengthPercentage::length(1.0),
                        bottom: LengthPercentage::length(3.0),
                    },
                    ..taffy_style()
                },
                Clip::None,
            ),
            (
                "margin_sides",
                |builder| {
                    builder
                        .margin_sides(px(1.0), px(2.0), px(3.0), px(4.0))
                        .build()
                },
                taffy::Style {
                    margin: Rect {
                        left: LengthPercentageAuto::length(4.0),
                        right: LengthPercentageAuto::length(2.0),
                        top: LengthPercentageAuto::length(1.0),
                        bottom: LengthPercentageAuto::length(3.0),
                    },
                    ..taffy_style()
                },
                Clip::None,
            ),
            (
                "layout_border",
                |builder| builder.border(px(1.0)).build(),
                taffy::Style {
                    border: Rect {
                        left: LengthPercentage::length(1.0),
                        right: LengthPercentage::length(1.0),
                        top: LengthPercentage::length(1.0),
                        bottom: LengthPercentage::length(1.0),
                    },
                    ..taffy_style()
                },
                Clip::None,
            ),
            (
                "width_height",
                |builder| builder.width(px(120.0)).height(px(40.0)).build(),
                taffy::Style {
                    size: TaffySize {
                        width: Dimension::length(120.0),
                        height: Dimension::length(40.0),
                    },
                    ..taffy_style()
                },
                Clip::None,
            ),
            (
                "size",
                |builder| builder.size(px(120.0), px(40.0)).build(),
                taffy::Style {
                    size: TaffySize {
                        width: Dimension::length(120.0),
                        height: Dimension::length(40.0),
                    },
                    ..taffy_style()
                },
                Clip::None,
            ),
            (
                "width_full_height_full",
                |builder| builder.width_full().height_full().build(),
                taffy::Style {
                    size: TaffySize {
                        width: Dimension::percent(1.0),
                        height: Dimension::percent(1.0),
                    },
                    ..taffy_style()
                },
                Clip::None,
            ),
            (
                "size_full",
                |builder| builder.size_full().build(),
                taffy::Style {
                    size: TaffySize {
                        width: Dimension::percent(1.0),
                        height: Dimension::percent(1.0),
                    },
                    ..taffy_style()
                },
                Clip::None,
            ),
            (
                "min_max",
                |builder| {
                    builder
                        .min_width_0()
                        .min_height_0()
                        .max_width(px(640.0))
                        .max_height(px(480.0))
                        .build()
                },
                taffy::Style {
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
                Clip::None,
            ),
            (
                "overflow_and_position",
                |builder| {
                    builder
                        .overflow_hidden()
                        .relative()
                        .top(px(1.0))
                        .right(px(2.0))
                        .bottom(px(3.0))
                        .left(px(4.0))
                        .build()
                },
                taffy::Style {
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
                Clip::Bounds,
            ),
        ];

        assert!(cases.len() >= 15, "抽样组合数必须 ≥15");
        for (name, build, expected, clip) in cases {
            let built = build(StyleBuilder::new());
            assert_eq!(built.inner(), &expected, "组合 {name} 的 taffy 字段");
            assert_eq!(built.clip, clip, "组合 {name} 的 clip");
        }
    }

    #[test]
    fn each_method_writes_only_its_own_fields() {
        let default = taffy_style();

        let column = StyleBuilder::new().flex_col().build();
        assert_eq!(column.inner().flex_direction, FlexDirection::Column);
        assert_eq!(column.inner().align_items, default.align_items);
        assert_eq!(column.inner().justify_content, default.justify_content);
        assert_eq!(column.inner().gap, default.gap);

        let centered = StyleBuilder::new().flex_row().items_center().build();
        assert_eq!(centered.inner().flex_direction, FlexDirection::Row);
        assert_eq!(centered.inner().align_items, AlignItems::CENTER);
        assert_eq!(centered.inner().justify_content, default.justify_content);

        let insets = StyleBuilder::new()
            .relative()
            .top(px(4.0))
            .left(px(8.0))
            .build();
        assert_eq!(insets.inner().position, Position::Relative);
        assert_eq!(insets.inner().inset.top, LengthPercentageAuto::length(4.0));
        assert_eq!(insets.inner().inset.left, LengthPercentageAuto::length(8.0));
        assert_eq!(insets.inner().inset.right, LengthPercentageAuto::auto());
        assert_eq!(insets.inner().inset.bottom, LengthPercentageAuto::auto());

        let absolute = StyleBuilder::new().absolute().build();
        assert_eq!(absolute.inner().position, Position::Absolute);
        assert_eq!(absolute.clip, Clip::None);
        assert_eq!(absolute.inner().overflow, default.overflow);
    }

    #[test]
    fn overflow_hidden_sets_both_the_engine_clip_and_the_taffy_overflow() {
        let style = StyleBuilder::new().overflow_hidden().build();

        assert_eq!(style.clip, Clip::Bounds);
        assert_eq!(
            style.inner().overflow,
            Point {
                x: Overflow::Hidden,
                y: Overflow::Hidden,
            }
        );
        assert_eq!(style.inner().size, taffy_style().size);
        assert_eq!(style.inner().padding, taffy_style().padding);
    }

    #[test]
    fn a_layout_border_takes_space_while_the_paint_border_draws() {
        let style = StyleBuilder::new().border(px(2.0)).build();
        for side in [
            style.inner().border.left,
            style.inner().border.right,
            style.inner().border.top,
            style.inner().border.bottom,
        ] {
            assert_eq!(side, LengthPercentage::length(2.0), "四边都占位");
        }
        assert_eq!(style.clip, Clip::None);

        assert!(
            PaintBuilder::new().build().border.is_none(),
            "布局边框不画线（画线是 PaintBuilder 的职责）"
        );

        let paint = PaintBuilder::new().border(px(2.0), Color::WHITE).build();
        assert_eq!(style.inner().border.top, LengthPercentage::length(2.0));
        assert_eq!(
            paint.border,
            Some(Border {
                width: px(2.0),
                color: Color::WHITE,
            })
        );
    }

    #[test]
    fn a_typical_panel_chain_matches_the_direct_style() {
        let built = StyleBuilder::new()
            .flex_col()
            .size_full()
            .gap(px(8.0))
            .padding(px(16.0))
            .items_center()
            .build();
        let expected = taffy::Style {
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
        };

        assert_eq!(built.inner(), &expected);
        assert_eq!(built.clip, Clip::None);
    }

    #[test]
    fn paint_builder_matches_the_direct_paint_spec() {
        let built = PaintBuilder::new()
            .background(Color::srgb(1.0, 0.0, 0.0, 1.0))
            .rounded(px(6.0))
            .border(px(1.0), Color::WHITE)
            .build();
        let expected = PaintSpec {
            background: Color::srgb(1.0, 0.0, 0.0, 1.0),
            border: Some(Border {
                width: px(1.0),
                color: Color::WHITE,
            }),
            corner_radius: [px(6.0); 4],
            shadows: Shadows::default(),
            gradient: None,
        };

        assert_eq!(built, expected);
        assert_eq!(PaintBuilder::default().build(), PaintSpec::default());
    }

    #[test]
    fn paint_builder_rounded_and_border_are_draw_only() {
        let paint = PaintBuilder::new()
            .rounded(px(4.0))
            .border(px(2.0), Color::WHITE)
            .build();

        assert_eq!(paint.corner_radius, [px(4.0); 4]);
        assert_eq!(
            paint.border,
            Some(Border {
                width: px(2.0),
                color: Color::WHITE,
            })
        );
        assert_eq!(paint.background, Color::TRANSPARENT);
        assert!(paint.shadows.is_empty());
        assert!(paint.gradient.is_none());
    }
}
