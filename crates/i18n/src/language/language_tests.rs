use super::codes::{CODES, LanguageCode};

#[test]
fn every_variant_maps_to_a_listed_code() {
    let mut mapped: Vec<_> = LanguageCode::all()
        .map(|code| (code.as_str(), code.name()))
        .collect();
    mapped.sort_unstable();

    let mut table = CODES.to_vec();
    table.sort_unstable();

    assert_eq!(mapped, table);
}

#[test]
fn every_listed_code_is_unique() {
    let mut codes: Vec<_> = CODES.iter().map(|(code, _)| *code).collect();
    codes.sort_unstable();
    codes.dedup();

    assert_eq!(codes.len(), CODES.len());
}

#[test]
fn every_listed_code_has_a_name() {
    assert!(CODES.iter().all(|(_, name)| !name.is_empty()));
    assert!(LanguageCode::all().all(|code| !code.name().is_empty()));
}

#[test]
fn parse_accepts_every_listed_code() {
    for (code, _) in CODES {
        let language = LanguageCode::parse(code).expect("the code is listed");
        assert_eq!(language.as_str(), *code);
    }
}

#[test]
fn parse_rejects_codes_outside_the_list() {
    assert_eq!(LanguageCode::parse("EN"), None);
    assert_eq!(LanguageCode::parse("en_US"), None);
    assert_eq!(LanguageCode::parse("en-US"), None);
    assert_eq!(LanguageCode::parse("he"), None);
    assert_eq!(LanguageCode::parse(""), None);
}

#[test]
fn parse_accepts_the_legacy_google_spellings() {
    assert_eq!(LanguageCode::parse("iw"), Some(LanguageCode::Iw));
    assert_eq!(LanguageCode::parse("in"), Some(LanguageCode::In));
    assert_eq!(LanguageCode::Iw.name(), "Hebrew");
    assert_eq!(LanguageCode::In.name(), "Indonesian");
}

#[test]
fn parse_reads_the_region_specific_codes() {
    assert_eq!(LanguageCode::parse("en"), Some(LanguageCode::En));
    assert_eq!(LanguageCode::parse("en-GB"), Some(LanguageCode::EnGb));
    assert_eq!(LanguageCode::parse("zh-CN"), Some(LanguageCode::ZhCn));
    assert_eq!(LanguageCode::parse("zh-TW"), Some(LanguageCode::ZhTw));
    assert_eq!(LanguageCode::parse("zh-HK"), Some(LanguageCode::ZhHk));
    assert_eq!(LanguageCode::parse("pt-BR"), Some(LanguageCode::PtBr));
    assert_eq!(LanguageCode::parse("es-419"), Some(LanguageCode::Es419));
}

#[test]
fn codes_render_as_their_google_spelling() {
    assert_eq!(LanguageCode::ZhCn.to_string(), "zh-CN");
    assert_eq!(LanguageCode::En.to_string(), "en");
}

#[test]
fn default_is_english() {
    assert_eq!(LanguageCode::default(), LanguageCode::En);
}

#[test]
fn all_matches_the_table_and_the_enum() {
    let codes: Vec<_> = LanguageCode::all().collect();
    let reachable = LanguageCode::all_reachable();

    assert_eq!(codes, reachable);
    assert_eq!(codes.len(), CODES.len());
}

#[test]
fn all_follows_the_language_table_order() {
    let expected: Vec<_> = CODES
        .iter()
        .map(|(code, _)| LanguageCode::parse(code).expect("the code is listed"))
        .collect();

    assert_eq!(LanguageCode::all().collect::<Vec<_>>(), expected);
}
