#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum WidgetCategory {
    Market,
    Statistics,
    Monitoring,
}

impl WidgetCategory {
    pub const ALL: [Self; 3] = [Self::Market, Self::Statistics, Self::Monitoring];

    pub const fn label(&self) -> &'static str {
        match self {
            Self::Market => "widget.categories.market",
            Self::Statistics => "widget.categories.statistics",
            Self::Monitoring => "widget.categories.monitoring",
        }
    }
}

#[cfg(test)]
mod tests {
    use ahash::HashSet;

    use super::WidgetCategory;

    #[test]
    fn every_category_should_name_itself_once() {
        let labels = WidgetCategory::ALL.map(|category| category.label());

        assert_eq!(
            labels.into_iter().collect::<HashSet<_>>().len(),
            WidgetCategory::ALL.len()
        );
        for label in labels {
            assert!(
                label.starts_with("widget.categories."),
                "`{label}` is not a category key"
            );
        }
    }
}
