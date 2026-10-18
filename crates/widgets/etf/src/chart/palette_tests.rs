//! The chart's geometry, and which colour a reading is drawn in.

use base_ui::style::theme::Theme;
use gpui_kit::{AbsoluteLength, TestAppContext, px, rems, size};

use super::palette::*;

#[test]
fn a_reading_should_be_coloured_by_the_direction_it_went() {
    let theme = Theme::default();
    let chart = &theme.components.chart;
    let muted = theme.roles.market.flat.hsla;

    assert_eq!(flow(&theme, 1.0, muted), chart.bullish.hsla);
    assert_eq!(flow(&theme, -1.0, muted), chart.bearish.hsla);
    assert_eq!(
        flow(&theme, 0.0, muted),
        muted,
        "a day that moved nothing has no direction"
    );
    assert_eq!(
        flow(&theme, f64::NAN, muted),
        muted,
        "a day that is not a reading has none either"
    );
    assert_eq!(
        flow(&theme, f64::NAN, theme.components.tooltip.foreground.hsla),
        theme.components.tooltip.foreground.hsla,
        "a day without a direction is read in the colour of the surface it is named on"
    );
}

#[test]
fn a_tooltip_should_be_sized_from_its_own_padding_and_lines() {
    let style = TooltipStyle {
        font_size: px(12.0),
        line_height: px(15.0),
        padding: px(4.0),
        radius: px(4.0),
    };

    assert_eq!(style.content_size(px(50.0), 3), size(px(58.0), px(53.0)));
}

/// The chart draws its tooltip on the canvas rather than mounting the
/// framework's, so nothing but a test keeps the box it draws the box the
/// framework draws a tooltip in.
#[gpui_kit::test]
fn a_chart_tooltip_should_be_the_box_the_framework_draws_one_in(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();

    cx.update(|window, _| {
        let style = tooltip_style(window);
        let rem_size = window.rem_size();

        assert_eq!(style.font_size, rems(0.75).to_pixels(rem_size), "text_xs");
        assert_eq!(
            style.padding,
            rems(0.25).to_pixels(rem_size),
            "p_1, the padding the framework's tooltip keeps on every edge"
        );
        assert_eq!(style.radius, rems(0.25).to_pixels(rem_size), "rounded_sm");
        assert_eq!(
            style.line_height,
            window
                .text_style()
                .line_height
                .to_pixels(AbsoluteLength::Pixels(style.font_size), rem_size),
            "a line of the chart's tooltip is as tall as a line the framework lays out"
        );
    });
}
