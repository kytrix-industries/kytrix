use std::sync::Arc;

use super::{DataStore, MessageStore};
use crate::{language::codes::LanguageCode, template::interpolate::Template};

fn store(entries: &[(&str, &str)]) -> MessageStore {
    let mut store = MessageStore::new(entries.len());
    for (key, text) in entries {
        assert!(store.insert(
            (*key).into(),
            Template::parse(text).expect("valid template")
        ));
    }
    store
}

#[test]
fn get_returns_the_registered_template() {
    let store = store(&[("menu.market", "Market"), ("menu.portfolio", "Portfolio")]);

    assert_eq!(
        store.get("menu.market"),
        Some(&Template::Static("Market".into()))
    );
    assert_eq!(
        store.get("menu.portfolio"),
        Some(&Template::Static("Portfolio".into()))
    );
    assert_eq!(store.get("menu.quant"), None);
    assert_eq!(store.len(), 2);
}

#[test]
fn inserting_a_duplicate_key_is_rejected() {
    let mut store = MessageStore::new(1);
    assert!(store.insert("a".into(), Template::Static("A".into())));
    assert!(!store.insert("a".into(), Template::Static("B".into())));
    assert_eq!(store.get("a"), Some(&Template::Static("A".into())));
}

#[test]
fn data_store_looks_up_by_language_code() {
    let data = DataStore::new(vec![
        (LanguageCode::En, Arc::new(store(&[("a", "A")]))),
        (LanguageCode::ZhCn, Arc::new(store(&[("a", "甲")]))),
    ]);

    assert_eq!(
        data.get(LanguageCode::ZhCn)
            .and_then(|store| store.get("a")),
        Some(&Template::Static("甲".into()))
    );
    assert!(data.get(LanguageCode::Ja).is_none());
}

#[test]
fn data_store_lists_loaded_languages() {
    let data = DataStore::new(vec![
        (LanguageCode::En, Arc::new(store(&[("a", "A")]))),
        (LanguageCode::ZhCn, Arc::new(store(&[("a", "甲")]))),
    ]);

    let mut languages: Vec<_> = data.languages().map(LanguageCode::as_str).collect();
    languages.sort_unstable();
    assert_eq!(languages, ["en", "zh-CN"]);
}
