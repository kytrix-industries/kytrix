use std::sync::Arc;

use ahash::{HashSet, HashSetExt};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use kytrix_proto::types::v1 as proto_types;
use kytrix_service_provider::service::engine::{FinancialIndexEvent, FinancialIndexTopic};
use serde::{Deserialize, Deserializer, Serialize};
use smol_str::{SmolStr, format_smolstr};
use thiserror::Error;
use widget::error::TranslatedError;

pub(super) const MAX_SELECTED_INDICES: usize = 256;
const MAX_MANIFEST_CATEGORIES: usize = 256;
const MAX_MANIFEST_INDICES: usize = 4_096;
const MAX_CODE_CHARS: usize = 64;
pub(super) const MAX_NAME_CHARS: usize = 120;

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(super) enum FinancialIndexDataError {
    #[error("the selection holds more than {limit} indices")]
    SelectionCapacity { limit: usize },
    #[error("the manifest holds more than {limit} entries")]
    ManifestCapacity { limit: usize },
    #[error("invalid index data: {0}")]
    Invalid(FinancialIndexInvalidReason),
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(super) enum FinancialIndexInvalidReason {
    #[error("the manifest carries an invalid code")]
    InvalidCode,
    #[error("the manifest repeats the category {code}")]
    DuplicateCategory { code: SmolStr },
    #[error("the manifest repeats the index {code}")]
    DuplicateFinancialIndex { code: SmolStr },
    #[error("index {index} references the undefined category {category}")]
    UnknownCategory { index: SmolStr, category: SmolStr },
    #[error("a manifest entry has no name")]
    MissingName,
    #[error("a name must be 1 to {MAX_NAME_CHARS} characters without surrounding whitespace")]
    NameFormat,
}

impl TranslatedError for FinancialIndexDataError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::SelectionCapacity { limit } => {
                t_smol!(
                    translate,
                    "error.financial_index.selection_capacity",
                    limit = limit
                )
            }
            Self::ManifestCapacity { limit } => {
                t_smol!(
                    translate,
                    "error.financial_index.manifest_too_large",
                    limit = limit
                )
            }
            Self::Invalid(reason) => reason.translated_message(translate),
        }
    }
}

impl TranslatedError for FinancialIndexInvalidReason {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::InvalidCode => t_smol!(translate, "error.financial_index.invalid_code"),
            Self::DuplicateCategory { code } => {
                t_smol!(
                    translate,
                    "error.financial_index.duplicate_category",
                    code = code
                )
            }
            Self::DuplicateFinancialIndex { code } => {
                t_smol!(
                    translate,
                    "error.financial_index.duplicate_index",
                    code = code
                )
            }
            Self::UnknownCategory { index, category } => t_smol!(
                translate,
                "error.financial_index.unknown_category",
                index = index,
                category = category
            ),
            Self::MissingName => t_smol!(translate, "error.financial_index.missing_name"),
            Self::NameFormat => {
                t_smol!(
                    translate,
                    "error.financial_index.name_format",
                    limit = MAX_NAME_CHARS
                )
            }
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub(super) struct LocalizedFinancialIndexName {
    pub en: SmolStr,
    pub zh: SmolStr,
}

impl LocalizedFinancialIndexName {
    pub(super) fn get(&self, language: LanguageCode) -> &str {
        match language {
            LanguageCode::ZhCn => &self.zh,
            _ => &self.en,
        }
    }

    fn validate(&self) -> Result<(), FinancialIndexDataError> {
        validate_name(&self.en)?;
        validate_name(&self.zh)
    }
}

impl<'de> Deserialize<'de> for LocalizedFinancialIndexName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            en: SmolStr,
            zh: SmolStr,
        }

        let wire = Wire::deserialize(deserializer)?;
        let name = Self {
            en: wire.en,
            zh: wire.zh,
        };
        name.validate().map_err(serde::de::Error::custom)?;
        Ok(name)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub(super) struct SelectedFinancialIndex {
    pub topic: FinancialIndexTopic,
    pub name: LocalizedFinancialIndexName,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FinancialIndexCategory {
    pub code: SmolStr,
    pub name: LocalizedFinancialIndexName,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FinancialIndexDescriptor {
    pub index: SelectedFinancialIndex,
    pub category: SmolStr,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FinancialIndexCatalog {
    pub categories: Arc<[FinancialIndexCategory]>,
    pub indices: Arc<[FinancialIndexDescriptor]>,
}

impl FinancialIndexCatalog {
    pub(super) fn descriptors_for(
        &self,
        category: Option<&SmolStr>,
        filter: Option<&str>,
    ) -> Arc<[FinancialIndexDescriptor]> {
        let Some(category) = category else {
            return Arc::from([]);
        };
        let filter = filter
            .map(str::trim)
            .filter(|filter| !filter.is_empty())
            .map(str::to_lowercase);
        let mut descriptors = Vec::with_capacity(self.indices.len());
        descriptors.extend(
            self.indices
                .iter()
                .filter(|descriptor| &descriptor.category == category)
                .filter(|descriptor| match filter.as_deref() {
                    Some(filter) => matches_filter(descriptor, filter),
                    None => true,
                })
                .cloned(),
        );
        descriptors.into()
    }
}

fn matches_filter(descriptor: &FinancialIndexDescriptor, filter: &str) -> bool {
    let index = &descriptor.index;
    [
        index.topic.code(),
        index.name.en.as_str(),
        index.name.zh.as_str(),
    ]
    .into_iter()
    .any(|field| field.to_lowercase().contains(filter))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FinancialIndexDirection {
    Down,
    Flat,
    Up,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FinancialIndexQuote {
    pub latest: u32,
    pub open: u32,
    pub is_trading: bool,
    pub timestamp: i64,
}

impl FinancialIndexQuote {
    pub(super) const fn change(self) -> i64 {
        self.latest as i64 - self.open as i64
    }

    pub(super) fn change_percent(self) -> Option<f64> {
        (self.open != 0).then(|| self.change() as f64 * 100.0 / f64::from(self.open))
    }

    pub(super) const fn direction(self) -> FinancialIndexDirection {
        if self.latest < self.open {
            FinancialIndexDirection::Down
        } else if self.latest > self.open {
            FinancialIndexDirection::Up
        } else {
            FinancialIndexDirection::Flat
        }
    }

    pub(super) const fn from_event(event: &FinancialIndexEvent) -> Self {
        Self {
            latest: event.last_price,
            open: event.open_price,
            is_trading: event.is_trading,
            timestamp: event.timestamp,
        }
    }
}

pub(super) fn prepare_catalog(
    manifest: proto_types::FinancialIndices,
) -> Result<FinancialIndexCatalog, FinancialIndexDataError> {
    ensure_manifest_capacity(manifest.categories.len(), MAX_MANIFEST_CATEGORIES)?;
    ensure_manifest_capacity(manifest.indices.len(), MAX_MANIFEST_INDICES)?;

    let mut category_codes = HashSet::with_capacity(manifest.categories.len());
    let mut categories = Vec::with_capacity(manifest.categories.len());
    for category in manifest.categories {
        let code = validate_code(&category.code)?;
        if !category_codes.insert(code.clone()) {
            return Err(invalid(FinancialIndexInvalidReason::DuplicateCategory {
                code,
            }));
        }
        categories.push(FinancialIndexCategory {
            code,
            name: parse_name(category.name)?,
        });
    }

    let mut index_codes = HashSet::with_capacity(manifest.indices.len());
    let mut indices = Vec::with_capacity(manifest.indices.len());
    for index in manifest.indices {
        validate_code(&index.request_code)?;
        let topic = FinancialIndexTopic::from_code(&index.code)
            .ok_or_else(|| invalid(FinancialIndexInvalidReason::InvalidCode))?;
        if !index_codes.insert(topic.clone()) {
            return Err(invalid(
                FinancialIndexInvalidReason::DuplicateFinancialIndex {
                    code: SmolStr::new(topic.code()),
                },
            ));
        }
        let category = validate_code(&index.category)?;
        if !category_codes.contains(&category) {
            return Err(invalid(FinancialIndexInvalidReason::UnknownCategory {
                index: SmolStr::new(topic.code()),
                category,
            }));
        }
        indices.push(FinancialIndexDescriptor {
            index: SelectedFinancialIndex {
                topic,
                name: parse_name(index.name)?,
            },
            category,
        });
    }

    Ok(FinancialIndexCatalog {
        categories: categories.into(),
        indices: indices.into(),
    })
}

pub(super) fn normalize_selection(
    selection: &[SelectedFinancialIndex],
) -> Result<Vec<SelectedFinancialIndex>, FinancialIndexDataError> {
    if selection.len() > MAX_SELECTED_INDICES {
        return Err(FinancialIndexDataError::SelectionCapacity {
            limit: MAX_SELECTED_INDICES,
        });
    }
    for index in selection {
        index.name.validate()?;
    }

    let mut seen = HashSet::with_capacity(selection.len());
    let mut normalized = Vec::with_capacity(selection.len());
    for index in selection {
        if seen.insert(index.topic.clone()) {
            normalized.push(index.clone());
        }
    }
    Ok(normalized)
}

pub(super) fn selection_topics(selection: &[SelectedFinancialIndex]) -> Arc<[FinancialIndexTopic]> {
    let mut topics = Vec::with_capacity(selection.len());
    topics.extend(selection.iter().map(|index| index.topic.clone()));
    topics.into()
}

pub(super) fn format_price(cents: u32) -> SmolStr {
    let major = cents / 100;
    let minor = cents % 100;
    format_smolstr!("{major}.{minor:02}")
}

pub(super) fn format_signed_cents(cents: i64) -> SmolStr {
    let magnitude = cents.unsigned_abs();
    let major = magnitude / 100;
    let minor = magnitude % 100;
    let sign = if cents < 0 {
        "-"
    } else if cents > 0 {
        "+"
    } else {
        ""
    };
    format_smolstr!("{sign}{major}.{minor:02}")
}

pub(super) fn format_percent(percent: Option<f64>) -> SmolStr {
    match percent {
        Some(value) if value > 0.0 => format_smolstr!("+{value:.2}%"),
        Some(value) => format_smolstr!("{value:.2}%"),
        None => SmolStr::new_static("--"),
    }
}

fn ensure_manifest_capacity(len: usize, limit: usize) -> Result<(), FinancialIndexDataError> {
    if len > limit {
        Err(FinancialIndexDataError::ManifestCapacity { limit })
    } else {
        Ok(())
    }
}

fn invalid(reason: FinancialIndexInvalidReason) -> FinancialIndexDataError {
    FinancialIndexDataError::Invalid(reason)
}

fn validate_code(code: &str) -> Result<SmolStr, FinancialIndexDataError> {
    if code.is_empty() || code.trim() != code || code.chars().count() > MAX_CODE_CHARS {
        return Err(invalid(FinancialIndexInvalidReason::InvalidCode));
    }
    Ok(SmolStr::new(code))
}

fn validate_name(value: &str) -> Result<(), FinancialIndexDataError> {
    if value.is_empty() || value.trim() != value || value.chars().count() > MAX_NAME_CHARS {
        return Err(invalid(FinancialIndexInvalidReason::NameFormat));
    }
    Ok(())
}

fn parse_name(
    name: Option<proto_types::FinancialIndexLocalizedName>,
) -> Result<LocalizedFinancialIndexName, FinancialIndexDataError> {
    let name = name.ok_or_else(|| invalid(FinancialIndexInvalidReason::MissingName))?;
    let name = LocalizedFinancialIndexName {
        en: SmolStr::new(name.en),
        zh: SmolStr::new(name.zh),
    };
    name.validate()?;
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topic(code: &str) -> FinancialIndexTopic {
        FinancialIndexTopic::from_code(code).expect("a test index code should be valid")
    }

    fn name(en: &str, zh: &str) -> Option<proto_types::FinancialIndexLocalizedName> {
        Some(proto_types::FinancialIndexLocalizedName {
            en: en.to_owned(),
            zh: zh.to_owned(),
        })
    }

    fn manifest() -> proto_types::FinancialIndices {
        proto_types::FinancialIndices {
            categories: vec![proto_types::FinancialIndexCategory {
                code: "asia".to_owned(),
                name: name("Asia", "亚洲"),
            }],
            indices: vec![proto_types::FinancialIndexDefinition {
                code: "HSI".to_owned(),
                category: "asia".to_owned(),
                name: name("Hang Seng Index", "恒生指数"),
                request_code: ".HSI".to_owned(),
            }],
        }
    }

    fn selected(code: &str) -> SelectedFinancialIndex {
        SelectedFinancialIndex {
            topic: topic(code),
            name: LocalizedFinancialIndexName {
                en: SmolStr::new(format_smolstr!("{code} English")),
                zh: SmolStr::new(format_smolstr!("{code} Chinese")),
            },
        }
    }

    fn quote(latest: u32, open: u32) -> FinancialIndexQuote {
        FinancialIndexQuote {
            latest,
            open,
            is_trading: true,
            timestamp: 1,
        }
    }

    #[test]
    fn a_quote_should_measure_its_move_against_the_open() {
        let event = FinancialIndexEvent {
            topic: topic("HSI"),
            last_price: 10_500,
            open_price: 10_000,
            high_price: 11_000,
            low_price: 9_500,
            close_price: 9_900,
            is_trading: true,
            timestamp: 123,
        };
        let quote = FinancialIndexQuote::from_event(&event);

        assert_eq!(
            quote,
            FinancialIndexQuote {
                latest: 10_500,
                open: 10_000,
                is_trading: true,
                timestamp: 123,
            }
        );
        assert_eq!(quote.change(), 500);
        assert_eq!(quote.change_percent(), Some(5.0));
        assert_eq!(quote.direction(), FinancialIndexDirection::Up);
    }

    #[test]
    fn a_zero_open_should_have_no_percentage_and_a_valid_direction() {
        let flat = quote(1, 0);

        assert_eq!(flat.change_percent(), None);
        assert_eq!(flat.direction(), FinancialIndexDirection::Up);
        assert_eq!(quote(0, 0).direction(), FinancialIndexDirection::Flat);
        assert_eq!(quote(0, 1).direction(), FinancialIndexDirection::Down);
    }

    #[test]
    fn catalog_should_keep_manifest_order_and_reject_invalid_references() {
        let catalog = prepare_catalog(manifest()).expect("the fixture manifest should be valid");
        assert_eq!(catalog.categories[0].code, "asia");
        assert_eq!(catalog.indices[0].index.topic.code(), "HSI");
        assert_eq!(catalog.categories[0].name.get(LanguageCode::ZhCn), "亚洲");
        assert_eq!(
            catalog.indices[0].index.name.get(LanguageCode::En),
            "Hang Seng Index"
        );

        let mut duplicate = manifest();
        duplicate.indices.push(duplicate.indices[0].clone());
        assert!(matches!(
            prepare_catalog(duplicate),
            Err(FinancialIndexDataError::Invalid(
                FinancialIndexInvalidReason::DuplicateFinancialIndex { code }
            )) if code == "HSI"
        ));

        let mut unknown = manifest();
        unknown.indices[0].category = "missing".to_owned();
        assert!(matches!(
            prepare_catalog(unknown),
            Err(FinancialIndexDataError::Invalid(
                FinancialIndexInvalidReason::UnknownCategory { index, category }
            )) if index == "HSI" && category == "missing"
        ));

        let mut duplicate_category = manifest();
        duplicate_category
            .categories
            .push(proto_types::FinancialIndexCategory {
                code: "asia".to_owned(),
                name: name("Asia again", "亚洲"),
            });
        assert!(matches!(
            prepare_catalog(duplicate_category),
            Err(FinancialIndexDataError::Invalid(
                FinancialIndexInvalidReason::DuplicateCategory { code }
            )) if code == "asia"
        ));
    }

    #[test]
    fn catalog_should_reject_a_code_the_service_could_not_use() {
        let mut untrimmed = manifest();
        untrimmed.indices[0].request_code = " .HSI".to_owned();
        assert!(matches!(
            prepare_catalog(untrimmed),
            Err(FinancialIndexDataError::Invalid(
                FinancialIndexInvalidReason::InvalidCode
            ))
        ));

        let mut overlong = manifest();
        overlong.indices[0].code = "x".repeat(MAX_CODE_CHARS + 1);
        assert!(matches!(
            prepare_catalog(overlong),
            Err(FinancialIndexDataError::Invalid(
                FinancialIndexInvalidReason::InvalidCode
            ))
        ));
    }

    #[test]
    fn catalog_should_reject_a_missing_or_overlong_name() {
        let mut missing = manifest();
        missing.indices[0].name = None;
        assert!(matches!(
            prepare_catalog(missing),
            Err(FinancialIndexDataError::Invalid(
                FinancialIndexInvalidReason::MissingName
            ))
        ));

        let mut overlong = manifest();
        overlong.indices[0].name = name(&"x".repeat(MAX_NAME_CHARS + 1), "valid");
        assert!(matches!(
            prepare_catalog(overlong),
            Err(FinancialIndexDataError::Invalid(
                FinancialIndexInvalidReason::NameFormat
            ))
        ));
    }

    #[test]
    fn catalog_should_accept_the_category_limit_and_reject_the_next_item() {
        let categories = (0..MAX_MANIFEST_CATEGORIES)
            .map(|index| proto_types::FinancialIndexCategory {
                code: format!("category_{index}"),
                name: name("Category", "分类"),
            })
            .collect::<Vec<_>>();
        let at_limit = prepare_catalog(proto_types::FinancialIndices {
            categories: categories.clone(),
            indices: Vec::new(),
        })
        .expect("the category limit should be accepted");
        assert_eq!(at_limit.categories.len(), MAX_MANIFEST_CATEGORIES);

        let mut over_limit = categories;
        over_limit.push(proto_types::FinancialIndexCategory {
            code: "one_too_many".to_owned(),
            name: name("Category", "分类"),
        });
        assert_eq!(
            prepare_catalog(proto_types::FinancialIndices {
                categories: over_limit,
                indices: Vec::new(),
            }),
            Err(FinancialIndexDataError::ManifestCapacity {
                limit: MAX_MANIFEST_CATEGORIES,
            })
        );
    }

    #[test]
    fn selection_should_preserve_first_seen_order_and_drop_duplicates() {
        let normalized = normalize_selection(&[
            selected("SPX"),
            selected("HSI"),
            selected("SPX"),
            selected("GOLD"),
        ])
        .expect("the selection should fit");

        assert_eq!(
            normalized
                .iter()
                .map(|index| index.topic.code())
                .collect::<Vec<_>>(),
            ["SPX", "HSI", "GOLD"]
        );
        assert_eq!(
            selection_topics(&normalized)
                .iter()
                .map(FinancialIndexTopic::code)
                .collect::<Vec<_>>(),
            ["SPX", "HSI", "GOLD"]
        );
    }

    #[test]
    fn selection_should_accept_the_hard_limit_and_reject_the_next_item() {
        let at_limit = (0..MAX_SELECTED_INDICES)
            .map(|index| selected(&format!("INDEX_{index}")))
            .collect::<Vec<_>>();
        assert_eq!(
            normalize_selection(&at_limit)
                .expect("the hard limit should be accepted")
                .len(),
            MAX_SELECTED_INDICES
        );

        let mut over_limit = at_limit;
        over_limit.push(selected("ONE_TOO_MANY"));
        assert_eq!(
            normalize_selection(&over_limit),
            Err(FinancialIndexDataError::SelectionCapacity {
                limit: MAX_SELECTED_INDICES,
            })
        );
    }

    #[test]
    fn selection_should_reject_a_persisted_name_the_manifest_could_not_carry() {
        let mut broken = selected("SPX");
        broken.name.en = SmolStr::new(" leading");

        assert!(matches!(
            normalize_selection(&[broken]),
            Err(FinancialIndexDataError::Invalid(
                FinancialIndexInvalidReason::NameFormat
            ))
        ));
    }

    #[test]
    fn catalog_should_list_only_the_indices_of_the_selected_category() {
        let mut manifest = manifest();
        manifest
            .categories
            .push(proto_types::FinancialIndexCategory {
                code: "america".to_owned(),
                name: name("America", "美洲"),
            });
        manifest
            .indices
            .push(proto_types::FinancialIndexDefinition {
                code: "SPX".to_owned(),
                category: "america".to_owned(),
                name: name("S&P 500", "标普 500"),
                request_code: ".SPX".to_owned(),
            });
        let catalog = prepare_catalog(manifest).expect("the fixture manifest should be valid");

        let america = catalog.descriptors_for(Some(&SmolStr::new_static("america")), None);
        assert_eq!(america.len(), 1);
        assert_eq!(america[0].index.topic.code(), "SPX");
        assert!(catalog.descriptors_for(None, None).is_empty());
        assert!(
            catalog
                .descriptors_for(Some(&SmolStr::new_static("missing")), None)
                .is_empty()
        );
    }

    #[test]
    fn a_search_should_match_an_index_by_its_code_or_either_of_its_names() {
        let catalog = prepare_catalog(manifest()).expect("the fixture manifest should be valid");
        let codes = |category: &str, filter: &str| {
            let category = SmolStr::new(category);
            catalog
                .descriptors_for(Some(&category), Some(filter))
                .iter()
                .map(|descriptor| descriptor.index.topic.code().to_owned())
                .collect::<Vec<_>>()
        };

        assert_eq!(codes("asia", "hsi"), ["HSI"], "a code matches in any case");
        assert_eq!(codes("asia", "Hang"), ["HSI"], "the English name matches");
        assert_eq!(codes("asia", "恒生"), ["HSI"], "the Chinese name matches");
        assert_eq!(
            codes("asia", "  hsi  "),
            ["HSI"],
            "an untrimmed search matches"
        );
        assert!(
            codes("asia", "SPX").is_empty(),
            "a search narrows the category rather than the catalogue: the S&P is an American index"
        );
        assert_eq!(
            codes("asia", ""),
            ["HSI"],
            "an empty search leaves the category it was typed in"
        );
        assert_eq!(codes("asia", "   "), ["HSI"], "so does a blank one");
    }

    #[test]
    fn a_search_should_leave_nothing_when_no_index_of_the_category_matches() {
        let catalog = prepare_catalog(manifest()).expect("the fixture manifest should be valid");

        assert!(
            catalog
                .descriptors_for(Some(&SmolStr::new_static("asia")), Some("nothing"))
                .is_empty()
        );
        assert!(
            catalog.descriptors_for(None, Some("hsi")).is_empty(),
            "a catalogue with no category selected offers nothing to search"
        );
    }

    #[test]
    fn formatting_should_keep_cents_and_explicit_signs() {
        assert_eq!(format_price(420_001), "4200.01");
        assert_eq!(format_price(7), "0.07");
        assert_eq!(format_signed_cents(500), "+5.00");
        assert_eq!(format_signed_cents(-500), "-5.00");
        assert_eq!(format_signed_cents(0), "0.00");
        assert_eq!(format_percent(Some(5.0)), "+5.00%");
        assert_eq!(format_percent(Some(-5.0)), "-5.00%");
        assert_eq!(format_percent(None), "--");
    }
}
