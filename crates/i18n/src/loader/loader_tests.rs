use std::path::Path;

use super::{Node, load_str};
use crate::{error::types::Error, template::interpolate::Template};

fn path() -> &'static Path {
    Path::new("en.yaml")
}

#[test]
fn leaves_become_dotted_keys() {
    let store = load_str(
        "greeting: Hello\nmenu:\n  market: Market\n  deep:\n    nested: Nested\n",
        path(),
    )
    .expect("valid locale");

    assert_eq!(
        store.get("greeting"),
        Some(&Template::Static("Hello".into()))
    );
    assert_eq!(
        store.get("menu.market"),
        Some(&Template::Static("Market".into()))
    );
    assert_eq!(
        store.get("menu.deep.nested"),
        Some(&Template::Static("Nested".into()))
    );
    assert_eq!(store.len(), 3);
}

#[test]
fn placeholders_are_parsed_while_loading() {
    let store = load_str("greeting: \"Hello, %{name}!\"\n", path()).expect("valid locale");
    let template = store.get("greeting").expect("the message exists");

    assert!(template.text().is_none());
    assert_eq!(template.placeholder_names().collect::<Vec<_>>(), ["name"]);
}

#[test]
fn numbers_flags_and_lists_are_not_messages() {
    for yaml in ["count: 3\n", "enabled: true\n", "items:\n  - a\n"] {
        let error = load_str(yaml, path()).expect_err("the entry is not a message");
        assert!(matches!(error, Error::NotAMessage { .. }), "{error}");
    }
}

#[test]
fn document_must_be_a_mapping() {
    let error = load_str("- a\n- b\n", path()).expect_err("a list is not a locale");
    assert!(matches!(error, Error::ParseLocaleFile { .. }), "{error}");
}

#[test]
fn unclosed_placeholder_names_its_key() {
    let error =
        load_str("greeting: \"Hello, %{name\"\n", path()).expect_err("the template is invalid");

    match error {
        Error::InvalidTemplate { key, .. } => assert_eq!(key, "greeting"),
        other => panic!("expected an invalid template error, got {other}"),
    }
}

#[test]
fn empty_document_is_rejected() {
    let error = load_str("\n", path()).expect_err("an empty document has no messages");
    assert!(matches!(error, Error::EmptyLocaleFile { .. }), "{error}");
}

#[test]
fn a_leaf_and_a_section_can_share_a_prefix() {
    let store = load_str("a: Leaf\na.b: Nested\n", path()).expect("valid locale");

    assert_eq!(store.get("a"), Some(&Template::Static("Leaf".into())));
    assert_eq!(store.get("a.b"), Some(&Template::Static("Nested".into())));
}

#[test]
fn quoted_scalars_keep_their_text() {
    let store = load_str("colon: \"a: b\"\nbrace: \"%{name}\"\n", path()).expect("valid locale");

    assert_eq!(store.get("colon"), Some(&Template::Static("a: b".into())));
    assert!(
        store
            .get("brace")
            .expect("the message exists")
            .text()
            .is_none()
    );
}

#[test]
fn node_visitor_is_reachable() {
    // Guards the visitor against a deserializer that resolves typed scalars,
    // which is what an untagged enum would have produced.
    let node: Node = serde_saphyr::from_str("42").expect("a scalar deserializes");
    assert!(matches!(node, Node::Unsupported));
}

#[test]
fn every_locale_file_in_the_fixture_directory_loads() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/locales");
    let locales = super::load_directory(&directory).expect("the fixture directory loads");

    assert_eq!(locales.len(), 3);
}
