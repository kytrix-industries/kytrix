use ahash::AHashSet;
use smol_str::SmolStr;

pub(crate) fn words(parts: &[&str]) -> String {
    let mut text = String::new();

    for part in parts {
        text.push_str(&part.to_lowercase());
        text.push(' ');
    }

    text
}

pub(crate) struct SearchNode {
    pub id: SmolStr,
    pub page: Option<PageSearch>,
    pub keywords: Vec<String>,
    pub children: Vec<SearchNode>,
}

pub(crate) struct PageSearch {
    pub label: String,
    pub fields: Vec<FieldSearch>,
}

pub(crate) struct FieldSearch {
    pub id: SmolStr,
    pub text: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) enum SearchFilter {
    #[default]
    All,
    Nothing,
    Matching {
        pages: AHashSet<SmolStr>,
        groups: AHashSet<SmolStr>,
        fields: AHashSet<SmolStr>,
    },
}

impl SearchFilter {
    pub fn new(query: &str, tree: &[SearchNode]) -> Self {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Self::All;
        }
        let tokens = query.split_whitespace().collect::<Vec<_>>();
        let mut filter = Self::Matching {
            pages: AHashSet::new(),
            groups: AHashSet::new(),
            fields: AHashSet::new(),
        };

        for node in tree {
            filter.admit(node, &tokens);
        }

        if filter.is_empty() {
            Self::Nothing
        } else {
            filter
        }
    }

    #[cfg(test)]
    pub fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }

    pub fn is_nothing(&self) -> bool {
        matches!(self, Self::Nothing)
    }

    pub fn admits_page(&self, id: &str) -> bool {
        match self {
            Self::All => true,
            Self::Nothing => false,
            Self::Matching { pages, .. } => pages.contains(id),
        }
    }

    pub fn admits_field(&self, page: &str, field_id: &str) -> bool {
        match self {
            Self::All => true,
            Self::Nothing => false,
            Self::Matching { pages, fields, .. } => {
                pages.contains(page) || fields.contains(field_id)
            }
        }
    }

    fn admit(&mut self, node: &SearchNode, tokens: &[&str]) {
        let Self::Matching {
            pages,
            groups,
            fields,
        } = self
        else {
            return;
        };
        let keywords = node
            .keywords
            .iter()
            .map(|keyword| keyword.to_lowercase())
            .collect::<Vec<_>>();

        match &node.page {
            None => {
                if tokens
                    .iter()
                    .all(|token| keywords.iter().any(|keyword| keyword.contains(token)))
                {
                    groups.insert(node.id.clone());
                    for child in &node.children {
                        self.admit_subtree(child);
                    }
                    return;
                }
                for child in &node.children {
                    self.admit(child, tokens);
                }
            }
            Some(page) => {
                let page_matches = tokens.iter().all(|token| {
                    page.label.contains(token)
                        || keywords.iter().any(|keyword| keyword.contains(token))
                });
                if page_matches {
                    pages.insert(node.id.clone());
                }
                for field in &page.fields {
                    if tokens.iter().all(|token| field.text.contains(token)) {
                        fields.insert(field.id.clone());
                    }
                }
            }
        }
    }

    fn admit_subtree(&mut self, node: &SearchNode) {
        let Self::Matching { pages, groups, .. } = self else {
            return;
        };

        match node.page {
            Some(_) => {
                pages.insert(node.id.clone());
            }
            None => {
                groups.insert(node.id.clone());
                for child in &node.children {
                    self.admit_subtree(child);
                }
            }
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::All | Self::Nothing => true,
            Self::Matching {
                pages,
                groups,
                fields,
            } => pages.is_empty() && groups.is_empty() && fields.is_empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FieldSearch, PageSearch, SearchFilter, SearchNode};

    fn tree() -> Vec<SearchNode> {
        vec![
            SearchNode {
                id: "appearance".into(),
                page: Some(PageSearch {
                    label: "appearance 外观".into(),
                    fields: vec![
                        FieldSearch {
                            id: "appearance".into(),
                            text: "appearance mode 外观模式 light dark system".into(),
                        },
                        FieldSearch {
                            id: "language".into(),
                            text: "language 语言 english".into(),
                        },
                    ],
                }),
                keywords: vec!["appearance".into(), "外观".into()],
                children: Vec::new(),
            },
            SearchNode {
                id: "proxy".into(),
                page: None,
                keywords: vec!["proxies".into(), "代理".into(), "socks5".into()],
                children: vec![SearchNode {
                    id: "server-management".into(),
                    page: Some(PageSearch {
                        label: "server management 服务器管理".into(),
                        fields: Vec::new(),
                    }),
                    keywords: vec!["proxy".into(), "server".into()],
                    children: Vec::new(),
                }],
            },
        ]
    }

    #[test]
    fn an_empty_query_should_admit_everything() {
        let filter = SearchFilter::new("  ", &tree());

        assert!(filter.is_all());
        assert!(filter.admits_page("server-management"));
        assert!(filter.admits_field("appearance", "language"));
    }

    #[test]
    fn a_query_that_matches_nothing_should_admit_nothing() {
        let filter = SearchFilter::new("nothing at all", &tree());

        assert!(filter.is_nothing());
        assert!(!filter.admits_page("appearance"));
        assert!(!filter.admits_page("server-management"));
        assert!(!filter.admits_field("appearance", "appearance"));
    }

    #[test]
    fn a_group_should_be_admitted_by_its_own_label_and_bring_its_children() {
        let filter = SearchFilter::new("代理", &tree());

        assert!(filter.admits_page("server-management"));
        assert!(!filter.admits_page("appearance"));
    }

    #[test]
    fn a_page_admitted_by_its_label_should_show_its_fields() {
        let filter = SearchFilter::new("外观", &tree());

        assert!(filter.admits_page("appearance"));
        assert!(filter.admits_field("appearance", "language"));
    }

    #[test]
    fn a_field_should_be_admitted_without_its_page_being_admitted() {
        let filter = SearchFilter::new("english", &tree());

        assert!(!filter.admits_page("appearance"));
        assert!(filter.admits_field("appearance", "language"));
        assert!(!filter.admits_field("appearance", "appearance"));
    }

    #[test]
    fn a_multi_word_query_should_require_every_word() {
        let filter = SearchFilter::new("appearance mode", &tree());

        assert!(filter.admits_field("appearance", "appearance"));
        assert!(!filter.admits_field("appearance", "language"));
    }

    #[test]
    fn a_query_should_be_matched_without_regard_to_case_or_surrounding_space() {
        let filter = SearchFilter::new("  ENGLISH ", &tree());

        assert!(filter.admits_field("appearance", "language"));
    }
}
