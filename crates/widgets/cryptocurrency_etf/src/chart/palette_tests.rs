use ui_core::{math::Size, paint::color::Color};
use ui_kit::theme::{
    appearance::Appearance, components::ComponentTokens, resolved::Theme, role::Role,
};

use super::palette::{flow, tooltip_style};

fn theme() -> Theme {
    Theme::of(Appearance::Light)
}

#[test]
fn a_reading_should_be_coloured_by_the_direction_it_went() {
    let theme = theme();
    let muted = theme.role(Role::MarketFlat);
    let chart = theme.components().chart;

    assert_eq!(flow(&theme, 12.0, muted), chart.bullish);
    assert_eq!(flow(&theme, -12.0, muted), chart.bearish);
    assert_eq!(
        flow(&theme, 0.0, muted),
        muted,
        "a day that moved nothing has no direction to colour"
    );
    assert_eq!(
        flow(&theme, f64::NAN, muted),
        muted,
        "a value that is not a reading is not coloured as one"
    );
    assert_ne!(
        chart.bullish, chart.bearish,
        "the two directions have to be told apart"
    );
}

#[test]
fn a_tooltip_should_be_sized_from_its_own_padding_and_lines() {
    let style = tooltip_style(&ComponentTokens::standard());

    assert_eq!(
        style.content_size(100.0, 3),
        Size::new(
            100.0 + style.padding * 2.0,
            style.line_height * 3.0 + style.padding * 2.0
        ),
        "every edge keeps the text away from it, top and bottom included"
    );
}

#[test]
fn a_chart_tooltip_should_be_the_size_the_frameworks_tooltip_draws_one_in() {
    let tokens = ComponentTokens::standard();
    let style = tooltip_style(&tokens);

    assert_eq!(
        style.font_size, tokens.small.font_size,
        "a tooltip line is the compact ladder's text size"
    );
    assert_eq!(
        style.line_height,
        tokens.small.font_size * 1.4,
        "and its line is the token's own line box"
    );
    assert!(style.radius > 0.0 && style.padding > 0.0);
    let theme = theme();
    let tooltip = theme.components().tooltip;
    assert_ne!(
        tooltip.background,
        Color::TRANSPARENT,
        "a tooltip is a surface, not a colourless hint"
    );
    assert_ne!(
        tooltip.foreground, tooltip.background,
        "its text has to be legible on its own surface"
    );
}
