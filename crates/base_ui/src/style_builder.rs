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
        self.style.inner.flex_direction = FlexDirection::Row;
        self
    }

    #[must_use]
    pub fn flex_col(mut self) -> Self {
        self.style.inner.flex_direction = FlexDirection::Column;
        self
    }

    #[must_use]
    pub fn flex_1(mut self) -> Self {
        self.style.inner.flex_grow = 1.0;
        self.style.inner.flex_shrink = 1.0;
        self.style.inner.flex_basis = Dimension::length(0.0);
        self
    }

    #[must_use]
    pub fn flex_none(mut self) -> Self {
        self.style.inner.flex_grow = 0.0;
        self.style.inner.flex_shrink = 0.0;
        self.style.inner.flex_basis = Dimension::auto();
        self
    }

    #[must_use]
    pub fn items_center(mut self) -> Self {
        self.style.inner.align_items = AlignItems::CENTER;
        self
    }

    #[must_use]
    pub fn justify_center(mut self) -> Self {
        self.style.inner.justify_content = JustifyContent::CENTER;
        self
    }

    #[must_use]
    pub fn justify_between(mut self) -> Self {
        self.style.inner.justify_content = JustifyContent::SPACE_BETWEEN;
        self
    }

    #[must_use]
    pub fn justify_end(mut self) -> Self {
        self.style.inner.justify_content = JustifyContent::END;
        self
    }

    #[must_use]
    pub fn gap(mut self, px: f32) -> Self {
        let gap = LengthPercentage::length(px);
        self.style.inner.gap = TaffySize {
            width: gap,
            height: gap,
        };
        self
    }

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

    #[must_use]
    pub fn padding_x(mut self, px: f32) -> Self {
        let value = LengthPercentage::length(px);
        self.style.inner.padding.left = value;
        self.style.inner.padding.right = value;
        self
    }

    #[must_use]
    pub fn padding_y(mut self, px: f32) -> Self {
        let value = LengthPercentage::length(px);
        self.style.inner.padding.top = value;
        self.style.inner.padding.bottom = value;
        self
    }

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

    #[must_use]
    pub fn width(mut self, px: f32) -> Self {
        self.style.inner.size.width = Dimension::length(px);
        self
    }

    #[must_use]
    pub fn height(mut self, px: f32) -> Self {
        self.style.inner.size.height = Dimension::length(px);
        self
    }

    #[must_use]
    pub fn size(mut self, width: f32, height: f32) -> Self {
        self.style.inner.size = TaffySize {
            width: Dimension::length(width),
            height: Dimension::length(height),
        };
        self
    }

    #[must_use]
    pub fn width_full(mut self) -> Self {
        self.style.inner.size.width = Dimension::percent(1.0);
        self
    }

    #[must_use]
    pub fn height_full(mut self) -> Self {
        self.style.inner.size.height = Dimension::percent(1.0);
        self
    }

    #[must_use]
    pub fn size_full(mut self) -> Self {
        self.style.inner.size = TaffySize {
            width: Dimension::percent(1.0),
            height: Dimension::percent(1.0),
        };
        self
    }

    #[must_use]
    pub fn min_width_0(mut self) -> Self {
        self.style.inner.min_size.width = LengthPercentageAuto::length(0.0);
        self
    }

    #[must_use]
    pub fn min_height_0(mut self) -> Self {
        self.style.inner.min_size.height = LengthPercentageAuto::length(0.0);
        self
    }

    #[must_use]
    pub fn max_width(mut self, px: f32) -> Self {
        self.style.inner.max_size.width = LengthPercentageAuto::length(px);
        self
    }

    #[must_use]
    pub fn max_height(mut self, px: f32) -> Self {
        self.style.inner.max_size.height = LengthPercentageAuto::length(px);
        self
    }

    #[must_use]
    pub fn overflow_hidden(mut self) -> Self {
        self.style.clip = Clip::Bounds;
        self.style.inner.overflow = Point {
            x: Overflow::Hidden,
            y: Overflow::Hidden,
        };
        self
    }

    #[must_use]
    pub fn relative(mut self) -> Self {
        self.style.inner.position = Position::Relative;
        self
    }

    #[must_use]
    pub fn absolute(mut self) -> Self {
        self.style.inner.position = Position::Absolute;
        self
    }

    #[must_use]
    pub fn top(mut self, px: f32) -> Self {
        self.style.inner.inset.top = LengthPercentageAuto::length(px);
        self
    }

    #[must_use]
    pub fn right(mut self, px: f32) -> Self {
        self.style.inner.inset.right = LengthPercentageAuto::length(px);
        self
    }

    #[must_use]
    pub fn bottom(mut self, px: f32) -> Self {
        self.style.inner.inset.bottom = LengthPercentageAuto::length(px);
        self
    }

    #[must_use]
    pub fn left(mut self, px: f32) -> Self {
        self.style.inner.inset.left = LengthPercentageAuto::length(px);
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
    pub fn rounded(mut self, px: f32) -> Self {
        self.paint.corner_radius = [px; 4];
        self
    }

    #[must_use]
    pub fn border(mut self, width: f32, color: Color) -> Self {
        self.paint.border = Some(Border { width, color });
        self
    }

    #[must_use]
    pub fn shadow(mut self, shadow: ShadowSpec) -> Self {
        self.paint.shadows = Shadows::from(shadow);
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
    use ui_core::paint::{
        color::Color,
        primitive::Border,
        style::{Clip, PaintSpec, ShadowSpec, Shadows, Style},
    };

    use super::{PaintBuilder, StyleBuilder};

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
        assert_eq!(style.inner.size, taffy_style().size);
        assert_eq!(style.inner.padding, taffy_style().padding);
    }

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

        assert!(
            PaintBuilder::new().build().border.is_none(),
            "布局边框不画线（画线是 PaintBuilder 的职责）"
        );

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
