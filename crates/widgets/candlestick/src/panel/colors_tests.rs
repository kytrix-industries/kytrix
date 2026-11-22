//! Which colours the panel offers, and what each picker is handed.
//!
//! The list is a function of the configuration ([`color_fields`]), so these are the answers the
//! built pickers are checked against in `smoke_tests`: one field per configurable colour, keyed by
//! the row it belongs to, carrying the colour the configuration holds.

use super::colors::{ColorField, ColorKey, color_fields, field_of};
use crate::indicator::{
    CandlestickIndicatorConfig, IndicatorId, LineColor, SettingControl, SettingKey, definition_of,
};

fn default_config() -> CandlestickIndicatorConfig {
    CandlestickIndicatorConfig::default()
}

fn fields_of(config: &CandlestickIndicatorConfig, id: IndicatorId) -> Vec<ColorField> {
    color_fields(config)
        .into_iter()
        .filter(|field| field.id == id)
        .collect()
}

/// The default configuration offers twelve colours: MA's three lines, EMA's one, RSI's three,
/// MACD's two and KDJ's three — and nothing offers a colour the settings do not hold.
#[test]
fn the_default_configuration_should_have_twelve_colour_fields() {
    let fields = color_fields(&default_config());

    assert_eq!(
        fields.len(),
        12,
        "the default configuration has twelve colours"
    );
    assert_eq!(
        fields_of(&default_config(), IndicatorId::Ma).len(),
        3,
        "MA's line list has three lines"
    );
    assert_eq!(fields_of(&default_config(), IndicatorId::Ema).len(), 1);
    assert_eq!(fields_of(&default_config(), IndicatorId::Rsi).len(), 3);
    assert_eq!(fields_of(&default_config(), IndicatorId::Macd).len(), 2);
    assert_eq!(fields_of(&default_config(), IndicatorId::Kdj).len(), 3);
}

/// A line list has one field per line, keyed by the line's row and carrying that line's colour.
#[test]
fn a_line_list_should_have_one_field_per_line_keyed_by_its_row() {
    let config = default_config();
    let lines = config
        .entry(IndicatorId::Ma)
        .and_then(|entry| entry.settings.lines())
        .expect("MA holds a line list");
    let fields = fields_of(&config, IndicatorId::Ma);

    assert_eq!(fields.len(), lines.rows.len());
    for (index, field) in fields.iter().enumerate() {
        assert_eq!(
            field.row,
            Some(index),
            "a line's colour is keyed by its row, not by its position in the drawn tree"
        );
        assert_eq!(
            field.key,
            SettingKey::Lines,
            "the field is the line list's own setting row"
        );
        assert_eq!(
            field.color, lines.rows[index].color,
            "the field carries line {index}'s stored colour"
        );
    }
}

/// A switch row offers the colour key it declares, and the settings hold that colour.
#[test]
fn a_switch_row_should_offer_the_colour_key_it_declares() {
    let config = default_config();
    let mut seen = 0;
    for entry in &config.entries {
        for row in definition_of(entry.id).rows {
            if !matches!(row.control, SettingControl::Switch) {
                continue;
            }
            let Some(key) = row.color_key else {
                continue;
            };
            let field = field_of(
                &config,
                ColorKey {
                    id: entry.id,
                    row: None,
                    key,
                },
            )
            .expect("every switch row's colour is listed");
            assert_eq!(field.row, None, "a switch row is not a line of a list");
            assert_eq!(
                Some(field.color),
                entry.settings.color(key),
                "the field reports the colour the settings hold"
            );
            seen += 1;
        }
    }
    assert!(seen > 0, "the default configuration has switch rows");
}

/// The field a picker is built for reports the stored colour, so a rebuild hands the picker the
/// value the configuration holds rather than the one it was created with.
#[test]
fn fields_should_report_the_colours_the_configuration_holds() {
    let mut config = default_config();
    let stored = LineColor {
        h: 0.33,
        s: 0.8,
        l: 0.4,
        a: 1.0,
    };
    assert!(
        config.set_line_color(IndicatorId::Ma, 1, stored),
        "MA's second line takes a colour"
    );

    let field = field_of(
        &config,
        ColorKey {
            id: IndicatorId::Ma,
            row: Some(1),
            key: SettingKey::Lines,
        },
    )
    .expect("the second line's colour is listed");

    assert_eq!(
        field.color, stored,
        "the picker is handed the stored colour"
    );
    assert_ne!(
        field.color,
        default_config()
            .entry(IndicatorId::Ma)
            .and_then(|entry| entry.settings.lines())
            .expect("MA holds a line list")
            .rows[1]
            .color,
        "the stored colour is not the default one"
    );
}
