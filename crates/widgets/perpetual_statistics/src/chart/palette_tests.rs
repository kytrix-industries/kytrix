use base_ui::style::theme::Theme;
use gpui_kit::{AbsoluteLength, TestAppContext, px, rems};

use super::palette::colors::*;

/// Every set of series one chart draws, in the order it draws them.
const CHART_SERIES: [[Series; 2]; 2] = [
    [Series::Amount, Series::Value],
    [Series::Long, Series::Short],
];
const RATIO_SERIES: [Series; 3] = [Series::Long, Series::Short, Series::Ratio];

#[test]
fn a_series_should_be_read_in_the_colour_of_its_role() {
    let theme = Theme::default();
    let colors = &theme.components.chart;
    let content = theme.roles.fg.default.hsla;

    assert_eq!(Series::Amount.color(colors, content), colors.series[0].hsla);
    assert_eq!(Series::Value.color(colors, content), colors.series[1].hsla);
    assert_eq!(
        Series::Long.color(colors, content),
        colors.bullish.hsla,
        "the long side is the market's up colour"
    );
    assert_eq!(
        Series::Short.color(colors, content),
        colors.bearish.hsla,
        "the short side is the market's down colour"
    );
    assert_eq!(
        Series::Ratio.color(colors, content),
        content,
        "the ratio is not one of the two sides: it is drawn in the colour the chart reads its content in, \
         so the swatch that names it and the line it names are the same colour"
    );
}

#[test]
fn every_series_of_one_chart_should_have_its_own_colour() {
    let theme = Theme::default();
    let chart = theme.components.chart;
    let content = theme.roles.fg.default.hsla;

    for drawn in CHART_SERIES {
        let colors = drawn.map(|series| series.color(&chart, content));
        assert_ne!(colors[0], colors[1]);
    }
    let ratio = RATIO_SERIES.map(|series| series.color(&chart, content));
    for (index, color) in ratio.iter().enumerate() {
        assert!(!ratio[index + 1..].contains(color));
    }
}

#[test]
fn a_tooltip_box_should_grow_with_its_lines() {
    let style = TooltipStyle {
        font_size: px(12.0),
        line_height: px(15.0),
        padding: px(4.0),
        radius: px(4.0),
    };

    let one = style.content_size(px(100.0), 1);
    let three = style.content_size(px(100.0), 3);

    assert_eq!(one.width, three.width);
    assert_eq!(three.height - one.height, px(30.0));
    assert_eq!(one.width, px(108.0));
    assert_eq!(
        one.height,
        px(23.0),
        "a tooltip is its lines with its own padding around them"
    );
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
