//! What a search query answers, over the tree and the fields in it.
//!
//! A query is answered once, into a filter the tree and the page are drawn against. What is
//! matched is the normalized text of every label, description and declared keyword: the
//! caller builds that text for the language it is drawn in, so a user looking for "代理" and
//! a user looking for "proxy" find the same setting.
//!
//! Matching is substring-based rather than fuzzy, and every word of a multi-word query has
//! to appear, which is what keeps "spot private" from answering with every key on every
//! market.

use ahash::AHashSet;
use gpui_kit::SharedString;

/// One node of the tree, with the text it is searched by.
pub(crate) struct SearchNode {
    pub id: SharedString,
    /// `None` for a group, whose children are searched separately.
    pub page: Option<PageSearch>,
    /// The words a group's own label is searched by, which for a page is its rendered label
    /// and the keywords it declares.
    pub keywords: Vec<String>,
    pub children: Vec<SearchNode>,
}

/// One page, with the text its label is searched by and the fields on it.
pub(crate) struct PageSearch {
    pub label: String,
    pub fields: Vec<FieldSearch>,
}

/// One field, with the text it is searched by.
pub(crate) struct FieldSearch {
    pub id: SharedString,
    pub text: String,
}

/// What one query admits.
///
/// An empty query is not an empty answer: it admits everything, which is why it is a case of
/// its own rather than a filter that happens to match.
#[derive(Clone, Debug, Default)]
pub(crate) enum SearchFilter {
    /// Nothing was typed, so nothing is filtered out.
    #[default]
    All,
    /// The query admitted nothing.
    Nothing,
    Matching {
        pages: AHashSet<SharedString>,
        groups: AHashSet<SharedString>,
        fields: AHashSet<SharedString>,
    },
}

impl SearchFilter {
    /// Answers one query over one tree.
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

    /// Whether anything is filtered out at all.
    pub fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }

    /// Whether the query admitted nothing at all.
    pub fn is_nothing(&self) -> bool {
        matches!(self, Self::Nothing)
    }

    /// Whether one page is admitted by its own label and keywords.
    ///
    /// A page can also be reached through one of its fields; that is a question about the page's
    /// fields rather than about its label, and the dialog asks both so a field match can open the
    /// page it sits on.
    pub fn admits_page(&self, id: &str) -> bool {
        match self {
            Self::All => true,
            Self::Nothing => false,
            Self::Matching { pages, .. } => pages.contains(id),
        }
    }

    /// Whether one field is drawn.
    ///
    /// A page that was admitted by its own label shows everything on it: a user searching
    /// for "proxy" is looking for that page rather than for an empty one. A page that is only
    /// reachable through its fields draws the fields the query matched and nothing else.
    pub fn admits_field(&self, page: &str, field_id: &str) -> bool {
        match self {
            Self::All => true,
            Self::Nothing => false,
            Self::Matching { pages, fields, .. } => {
                pages.contains(page) || fields.contains(field_id)
            }
        }
    }

    /// Admits a node and, when it is a group, everything under it.
    ///
    /// A group is admitted by its own label, which is what lets a user reach the pages under
    /// it without knowing what they are called; a page is admitted by its label, and its fields
    /// are admitted separately, which is what lets a field match reach the page it sits on.
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
                    // A group that was admitted brings everything under it: a user searching
                    // for the group is looking for the group, not for the pages of it that
                    // happen to repeat the word.
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

    /// Admits one node and everything under it, without asking the query again.
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

    /// A tree with one group over one page and one page of its own, written the way the
    /// catalog is: the group's label is its first keyword.
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
                    id: "proxy-management".into(),
                    page: Some(PageSearch {
                        label: "proxy management 代理管理".into(),
                        fields: Vec::new(),
                    }),
                    keywords: vec!["proxy".into(), "catalog".into()],
                    children: Vec::new(),
                }],
            },
        ]
    }

    #[test]
    fn an_empty_query_should_admit_everything() {
        let filter = SearchFilter::new("  ", &tree());

        assert!(filter.is_all());
        assert!(filter.admits_page("proxy-management"));
        assert!(filter.admits_field("appearance", "language"));
    }

    #[test]
    fn a_query_that_matches_nothing_should_admit_nothing() {
        let filter = SearchFilter::new("nothing at all", &tree());

        assert!(matches!(filter, SearchFilter::Nothing));
        assert!(!filter.admits_page("appearance"));
        assert!(!filter.admits_page("proxy-management"));
        assert!(!filter.admits_field("appearance", "appearance"));
    }

    /// A group is searched by its own label rather than by the pages under it, so a query that
    /// names the group brings everything it holds — which is what makes a group a way in.
    #[test]
    fn a_group_should_be_admitted_by_its_own_label_and_bring_its_children() {
        let filter = SearchFilter::new("代理", &tree());

        assert!(filter.admits_page("proxy-management"));
        assert!(!filter.admits_page("appearance"));
    }

    /// A page admitted by its own label shows everything on it, which is what makes a search
    /// for a page name useful rather than a way to reach an empty page.
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

    /// Every word has to appear, so a second word narrows the answer rather than widening it.
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
