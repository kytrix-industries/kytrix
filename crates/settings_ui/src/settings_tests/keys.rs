use std::path::Path;

const TEST_MODULE_ATTRIBUTE: &str = concat!("#[cfg", "(test)]");

const TEST_MODULE_SHAPES: [&str; 4] = ["mod ", "pub mod ", "pub(crate) mod ", "pub(super) mod "];

const TEST_SOURCES_DIRECTORY: &str = "settings_tests";

const SETTINGS_SOURCES: [(&str, &str); 31] = [
    ("appearance.rs", include_str!("../appearance.rs")),
    ("host.rs", include_str!("../host.rs")),
    ("layout.rs", include_str!("../layout.rs")),
    ("lib.rs", include_str!("../lib.rs")),
    ("pages/about.rs", include_str!("../pages/about.rs")),
    (
        "pages/appearance.rs",
        include_str!("../pages/appearance.rs"),
    ),
    ("pages/configure.rs", include_str!("../pages/configure.rs")),
    (
        "pages/data_format.rs",
        include_str!("../pages/data_format.rs"),
    ),
    ("pages/mod.rs", include_str!("../pages/mod.rs")),
    ("pages/page.rs", include_str!("../pages/page.rs")),
    (
        "pages/proxy/editor.rs",
        include_str!("../pages/proxy/editor.rs"),
    ),
    ("pages/proxy/mod.rs", include_str!("../pages/proxy/mod.rs")),
    (
        "pages/proxy/route.rs",
        include_str!("../pages/proxy/route.rs"),
    ),
    (
        "pages/proxy/table.rs",
        include_str!("../pages/proxy/table.rs"),
    ),
    ("pages/registry.rs", include_str!("../pages/registry.rs")),
    (
        "pages/translation/mod.rs",
        include_str!("../pages/translation/mod.rs"),
    ),
    (
        "pages/translation/page.rs",
        include_str!("../pages/translation/page.rs"),
    ),
    (
        "pages/translation/table.rs",
        include_str!("../pages/translation/table.rs"),
    ),
    (
        "pages/venues/binance.rs",
        include_str!("../pages/venues/binance.rs"),
    ),
    (
        "pages/venues/credentials.rs",
        include_str!("../pages/venues/credentials.rs"),
    ),
    (
        "pages/venues/dispatcher.rs",
        include_str!("../pages/venues/dispatcher.rs"),
    ),
    (
        "pages/venues/hyperliquid.rs",
        include_str!("../pages/venues/hyperliquid.rs"),
    ),
    (
        "pages/venues/longbridge.rs",
        include_str!("../pages/venues/longbridge.rs"),
    ),
    (
        "pages/venues/mod.rs",
        include_str!("../pages/venues/mod.rs"),
    ),
    (
        "pages/venues/okx.rs",
        include_str!("../pages/venues/okx.rs"),
    ),
    (
        "pages/venues/polymarket.rs",
        include_str!("../pages/venues/polymarket.rs"),
    ),
    (
        "pages/venues/shared.rs",
        include_str!("../pages/venues/shared.rs"),
    ),
    ("search.rs", include_str!("../search.rs")),
    ("session.rs", include_str!("../session.rs")),
    ("settings.rs", include_str!("../settings.rs")),
    (
        "translation_downloads.rs",
        include_str!("../translation_downloads.rs"),
    ),
];

fn production_half(source: &'static str) -> &'static str {
    let mut offset = 0usize;

    while let Some(attribute) = source[offset..].find(TEST_MODULE_ATTRIBUTE) {
        offset += attribute + TEST_MODULE_ATTRIBUTE.len();
        let declaration = source[offset..].trim_start();
        if TEST_MODULE_SHAPES
            .iter()
            .any(|shape| declaration.starts_with(shape))
        {
            return &source[..offset - TEST_MODULE_ATTRIBUTE.len()];
        }
    }

    source
}

fn declared_keys() -> Vec<&'static str> {
    SETTINGS_SOURCES
        .iter()
        .flat_map(|(_, source)| production_half(source).split('"').skip(1).step_by(2))
        .filter(|literal| literal.starts_with("settings_ui."))
        .collect()
}

fn source_names_on_disk() -> Vec<String> {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
    let mut names = Vec::new();

    collect_sources(Path::new(root), "", &mut names);

    names.sort();
    names
}

fn collect_sources(directory: &Path, prefix: &str, names: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries {
        let entry = entry.expect("a directory entry is readable");
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let path = entry.path();

        if path.is_dir() {
            if name.as_ref() != TEST_SOURCES_DIRECTORY {
                collect_sources(&path, &format!("{prefix}{name}/"), names);
            }
        } else if name.ends_with(".rs") {
            names.push(format!("{prefix}{name}"));
        }
    }
}

#[test]
fn every_settings_source_should_be_read_by_the_key_guard() {
    let declared = SETTINGS_SOURCES
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .collect::<Vec<_>>();

    assert_eq!(
        declared,
        source_names_on_disk(),
        "the guard reads the crate's production sources, and the crate's production sources are \
         these"
    );
}

#[test]
fn every_key_the_settings_name_should_be_defined_in_every_locale() {
    let i18n = i18n::embedded::load().expect("embedded locales should load");
    let keys = declared_keys();

    assert!(
        keys.len() > 50,
        "the settings should name their labels, and {} was read out of them",
        keys.len()
    );

    for language in i18n.languages() {
        for key in &keys {
            assert!(
                i18n.try_translate(language, key)
                    .is_some_and(|text| !text.is_empty()),
                "{key} is named by the settings but {language} does not define it"
            );
        }
    }
}

#[test]
fn every_settings_key_is_named_by_a_source() {
    let i18n = i18n::embedded::load().expect("embedded locales should load");
    let drawn = declared_keys();

    for language in i18n.languages() {
        let defined = i18n
            .keys(language)
            .expect("an embedded language exposes its keys")
            .filter(|key| key.starts_with("settings_ui."))
            .collect::<Vec<_>>();

        for key in &defined {
            assert!(
                drawn.iter().any(|drawn| drawn == key),
                "{key} is defined in {language} but nothing in the settings names it"
            );
        }
    }
}
