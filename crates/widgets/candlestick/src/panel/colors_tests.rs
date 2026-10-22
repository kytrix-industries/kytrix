//! Tests for the panel's keyed table of colour pickers.
//!
//! The table is the panel's answer to "which picker edits which colour": every
//! field the configuration has is one entry, keyed by the indicator and line it
//! belongs to, and it shows the colour the configuration stores.

use gpui_kit::TestAppContext;

use super::colors::{color_fields, color_labels};
use crate::indicator::{
    CandlestickIndicatorConfig, INDICATOR_CATALOG, IndicatorId, LineColor, SettingControl,
    SettingKey,
};

#[test]
fn the_default_configuration_should_have_twelve_colour_fields() {
    let config = CandlestickIndicatorConfig::default();
    let fields = color_fields(&config);

    assert_eq!(
        fields.len(),
        12,
        "three MA lines, one EMA line, three RSI lines, MACD's two switch colours and KDJ's three"
    );
    for (index, field) in fields.iter().enumerate() {
        let key = field.key();
        assert!(
            !fields[..index].iter().any(|earlier| earlier.key() == key),
            "{key:?} is listed twice"
        );
    }
    for (id, count) in [
        (IndicatorId::Ma, 3),
        (IndicatorId::Ema, 1),
        (IndicatorId::Vol, 0),
        (IndicatorId::Macd, 2),
        (IndicatorId::Rsi, 3),
        (IndicatorId::Kdj, 3),
    ] {
        assert_eq!(
            fields.iter().filter(|field| field.id == id).count(),
            count,
            "{id:?} offers the wrong number of colour fields"
        );
    }
}

#[test]
fn a_line_list_should_have_one_field_per_line_keyed_by_its_row() {
    let config = CandlestickIndicatorConfig::default();
    let fields = color_fields(&config);

    for definition in INDICATOR_CATALOG {
        for row in definition.rows {
            if !matches!(row.control, SettingControl::LineList { .. }) {
                continue;
            }
            let lines = config
                .entry(definition.id)
                .expect("the catalogue always has the entry")
                .settings
                .lines()
                .expect("a line-list row edits a line list");
            for (index, line) in lines.rows.iter().enumerate() {
                let field = fields
                    .iter()
                    .find(|field| {
                        field.id == definition.id
                            && field.row == Some(index)
                            && field.key == row.key
                    })
                    .unwrap_or_else(|| {
                        panic!("{:?}'s line {index} has no colour field", definition.id)
                    });
                assert_eq!(
                    field.color, line.color,
                    "{:?}'s line {index} shows the colour it stores",
                    definition.id
                );
            }
        }
    }
}

#[test]
fn a_switch_row_should_offer_the_colour_key_it_declares() {
    let config = CandlestickIndicatorConfig::default();
    let fields = color_fields(&config);
    let switch_fields: Vec<_> = fields.iter().filter(|field| field.row.is_none()).collect();
    assert_eq!(
        switch_fields.len(),
        5,
        "MACD's DIF and DEA and KDJ's K, D and J are the switch rows with a colour"
    );

    for definition in INDICATOR_CATALOG {
        for row in definition.rows {
            if !matches!(row.control, SettingControl::Switch) {
                continue;
            }
            let key = row
                .color_key
                .expect("every switch row declares the colour it controls");
            let field = fields
                .iter()
                .find(|field| field.id == definition.id && field.row.is_none() && field.key == key)
                .unwrap_or_else(|| {
                    panic!(
                        "{:?}'s {:?} row offers no colour field",
                        definition.id, row.key
                    )
                });
            let stored = config
                .entry(definition.id)
                .expect("the catalogue always has the entry")
                .settings
                .color(key)
                .expect("the row's colour key names a stored field");
            assert_eq!(
                field.color, stored,
                "{:?}::{key:?} shows the colour it stores",
                definition.id
            );
        }
    }
}

#[test]
fn fields_should_report_the_colours_the_configuration_holds() {
    let line_color = LineColor {
        h: 0.11,
        s: 0.22,
        l: 0.33,
        a: 0.44,
    };
    let switch_color = LineColor {
        h: 0.55,
        s: 0.66,
        l: 0.77,
        a: 0.88,
    };
    let mut config = CandlestickIndicatorConfig::default();
    assert!(config.set_line_color(IndicatorId::Rsi, 1, line_color));
    assert!(config.set_color(IndicatorId::Kdj, SettingKey::DColor, switch_color));

    let fields = color_fields(&config);

    let line_field = fields
        .iter()
        .find(|field| field.id == IndicatorId::Rsi && field.row == Some(1))
        .expect("RSI's second line has a field");
    assert_eq!(
        line_field.color, line_color,
        "the field shows the exact stored line colour, alpha included"
    );

    let switch_field = fields
        .iter()
        .find(|field| field.id == IndicatorId::Kdj && field.key == SettingKey::DColor)
        .expect("KDJ's D row has a field");
    assert_eq!(
        switch_field.color, switch_color,
        "the field shows the exact stored switch colour"
    );
}

/// The picker announces the panel in the language of the frame being drawn.
///
/// Only the lookup is pinned here. Whether `sync_pickers` hands these labels to
/// the picker cannot be asserted: `ColorPicker` keeps its labels private and has
/// no getter, so the call itself is covered by construction rather than by a
/// test.
#[gpui_kit::test]
fn the_picker_labels_should_be_the_english_set(cx: &mut TestAppContext) {
    cx.update(|cx| {
        i18n::global::embedded::install(cx).expect("embedded locales should load");
    });
    let labels = cx.update(|cx| color_labels(cx));

    assert_eq!(labels.palette.as_ref(), "Palette");
    assert_eq!(labels.hsla.as_ref(), "HSLA");
    assert_eq!(labels.hue.as_ref(), "Hue");
    assert_eq!(labels.saturation.as_ref(), "Saturation");
    assert_eq!(labels.lightness.as_ref(), "Lightness");
    assert_eq!(labels.alpha.as_ref(), "Opacity");
}
