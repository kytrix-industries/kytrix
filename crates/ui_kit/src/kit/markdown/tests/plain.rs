use super::{
    EN_RELEASE_NOTES, ZH_RELEASE_NOTES,
    fixtures::{
        i18n_markdown::I18N_MARKDOWN,
        legacy_plain_text::{
            LEGACY_DOCUMENT_PLAIN_TEXT, LEGACY_PLAIN_TEXT, LEGACY_PLAIN_TEXT_FOLDED,
            PRODUCTION_PLAIN_TEXT_SOURCES,
        },
    },
};
use crate::kit::markdown::plain_text;

fn source_of(key: &str) -> &'static str {
    if let Some((language, name)) = key.split_once(':')
        && matches!(language, "en" | "zh")
        && let Some(case) = I18N_MARKDOWN.iter().find(|case| case.key == name)
    {
        return if language == "en" { case.en } else { case.zh };
    }
    if let Some((_, source)) = PRODUCTION_PLAIN_TEXT_SOURCES
        .iter()
        .find(|(candidate, _)| *candidate == key)
    {
        return source;
    }
    match key {
        "release:en" => EN_RELEASE_NOTES,
        "release:zh" => ZH_RELEASE_NOTES,
        other => panic!("the fixture key {other} has no source"),
    }
}

fn expected_of(key: &str) -> &'static str {
    LEGACY_PLAIN_TEXT
        .iter()
        .find(|(candidate, _)| *candidate == key)
        .map(|(_, expected)| *expected)
        .unwrap_or_else(|| panic!("the fixture key {key} has no expectation"))
}

#[test]
fn plain_text_joins_blocks_and_folds_blank_lines() {
    assert_eq!(plain_text("a\n\nb").as_str(), "a\nb");
    assert_eq!(plain_text("a\n\n\n\nb").as_str(), "a\nb");
    assert_eq!(plain_text("a  \nb").as_str(), "a\nb");
    assert_eq!(plain_text("> quoted").as_str(), "quoted");
    assert_eq!(plain_text("- a\n- b").as_str(), "a\nb");
    assert_eq!(plain_text("```\nx\ny\n```").as_str(), "x\ny");
    assert_eq!(
        plain_text("| a | b |\n|---|---|\n| 1 | 2 |").as_str(),
        "a b\n1 2"
    );
    assert_eq!(plain_text("| a | | c |\n|---|---|---|").as_str(), "a  c");
    assert_eq!(plain_text("a  \nb  \nc").as_str(), "a\nb\nc");
    assert_eq!(plain_text("a\\\n\\\nb").as_str(), "a\n\nb");
    assert_eq!(plain_text("<kbd>Ctrl</kbd><br>K").as_str(), "Ctrl\nK");
    assert_eq!(plain_text("<p>x</p>b<br>c").as_str(), "x\nb\nc");
    assert_eq!(plain_text("<div>x</div>a<br>b").as_str(), "x\na\nb");
    assert_eq!(plain_text("<p>a</p>x<p>b</p>y").as_str(), "a\nb\nx\ny");
    assert_eq!(plain_text("![alt](file://x.png)").as_str(), "");
    assert_eq!(plain_text("<p align=\"center\">x</p>").as_str(), "x");
    assert_eq!(plain_text("").as_str(), "");
}

#[test]
fn the_legacy_plain_text_baseline_keeps_seventy_nine_literals_frozen() {
    assert_eq!(LEGACY_PLAIN_TEXT.len(), 81);
    let mut checked = 0;
    for (key, expected) in LEGACY_PLAIN_TEXT {
        if key.starts_with("release:") {
            continue;
        }
        assert_eq!(plain_text(source_of(key)).as_str(), *expected, "{key}");
        checked += 1;
    }
    assert_eq!(checked, 79);
}

#[test]
fn the_two_production_plain_text_inputs_match_their_frozen_literals() {
    for key in [
        "en:prod:widget.watch_list.market_metadata_not_loaded",
        "en:prod:settings_ui.proxies.delete_message",
        "zh:prod:widget.watch_list.market_metadata_not_loaded",
        "zh:prod:settings_ui.proxies.delete_message",
    ] {
        assert_eq!(
            plain_text(source_of(key)).as_str(),
            expected_of(key),
            "{key}"
        );
    }
}

#[test]
fn plain_text_deviation_08_folds_the_release_note_blank_lines() {
    assert_eq!(LEGACY_PLAIN_TEXT_FOLDED.len(), 2);
    for (key, expected) in LEGACY_PLAIN_TEXT_FOLDED {
        let legacy = expected_of(key);
        assert!(legacy.contains("\n\n"), "{key}");
        let actual = plain_text(source_of(key));
        assert_eq!(actual.as_str(), *expected, "{key}");
        assert!(!actual.contains("\n\n"), "{key}");
    }
}

#[test]
fn plain_text_deviation_08_folds_the_document_baseline_blank_lines() {
    assert_eq!(LEGACY_DOCUMENT_PLAIN_TEXT.len(), 2);
    for (key, source, legacy, frozen) in LEGACY_DOCUMENT_PLAIN_TEXT {
        assert!(legacy.contains("\n\n"), "{key}");
        assert_eq!(plain_text(source).as_str(), *frozen, "{key}");
        assert_eq!(
            legacy
                .chars()
                .filter(|character| *character != '\n')
                .collect::<String>(),
            frozen
                .chars()
                .filter(|character| *character != '\n')
                .collect::<String>(),
            "{key}"
        );
    }
}
