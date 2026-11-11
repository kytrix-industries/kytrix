//! Every page the settings offer, and the search index over them.
//!
//! This is navigation metadata, not a description of a page's UI: a page is named, placed in the
//! sidebar and given the words a search reaches it by, and its module draws the page itself. A
//! new setting is added in the page module that holds it; only a new page is added here.

use gpui_kit::{AnyElement, App, SharedString};
use i18n::t;
use market_participants_connector::venue::definition::supported_venues;
use market_participants_model::identifier::VenueId;
use widget::venue::VenueDisplay;

use crate::{
    layout::PageArgs,
    pages::{about, appearance, configure, data_format, venues},
    search::{PageSearch, SearchFilter, SearchNode, words},
};

/// The page the settings open on.
pub(crate) const DEFAULT_PAGE: &str = "appearance";

/// One page of the settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PageId {
    Appearance,
    DataFormat,
    Translation,
    ServerManagement,
    Configure,
    Venue(VenueId),
    About,
}

impl PageId {
    /// The id a selection and a search both name this page by.
    pub(crate) fn id(self) -> SharedString {
        match self {
            Self::Appearance => SharedString::new_static("appearance"),
            Self::DataFormat => SharedString::new_static("data-format"),
            Self::Translation => SharedString::new_static("ai.translation"),
            Self::ServerManagement => SharedString::new_static("server-management"),
            Self::Configure => SharedString::new_static("configure"),
            Self::Venue(venue) => {
                SharedString::from(format!("market_participants.{}", venue.as_str()))
            }
            Self::About => SharedString::new_static("about"),
        }
    }

    /// The name this page is drawn with.
    pub(crate) fn title(self, cx: &App) -> SharedString {
        match self {
            Self::Appearance => t!(cx, "settings_ui.appearance.title"),
            Self::DataFormat => t!(cx, "settings_ui.data_format.title"),
            Self::Translation => t!(cx, "settings_ui.ai.translation.title"),
            Self::ServerManagement => t!(cx, "settings_ui.proxies.server_management"),
            Self::Configure => t!(cx, "settings_ui.proxies.configure"),
            Self::Venue(venue) => t!(cx, venue.name_key()),
            Self::About => t!(cx, "settings_ui.about.title"),
        }
    }

    /// The text this page and its settings are searched by.
    pub(crate) fn search(self, cx: &App) -> PageSearch {
        match self {
            Self::Appearance => appearance::search(cx),
            Self::DataFormat => data_format::search(cx),
            Self::Translation => crate::pages::translation::page::search(cx),
            Self::ServerManagement => {
                let title = t!(cx, "settings_ui.proxies.server_management");

                PageSearch {
                    label: words(&[
                        title.as_ref(),
                        "proxy",
                        "server",
                        "management",
                        "socks5",
                        "代理",
                        "服务器",
                        "管理",
                    ]),
                    fields: Vec::new(),
                }
            }
            Self::Configure => configure::search(cx),
            Self::Venue(venue) => venues::dispatcher::search(venue, cx),
            Self::About => about::search(cx),
        }
    }

    /// Whether this page is the translation view.
    pub(crate) fn is_translation(self) -> bool {
        matches!(self, Self::Translation)
    }
}

/// One heading of the sidebar and the pages under it.
pub(crate) struct Group {
    pub id: &'static str,
    pub label_key: &'static str,
    pub keywords: &'static [&'static str],
    pub pages: Vec<PageId>,
}

/// The groups of the sidebar, in the order it draws them.
pub(crate) fn groups() -> Vec<Group> {
    vec![
        Group {
            id: "general",
            label_key: "settings_ui.general.title",
            keywords: &["general", "常规", "设置"],
            pages: vec![PageId::Appearance, PageId::DataFormat],
        },
        Group {
            id: "ai",
            label_key: "settings_ui.ai.title",
            keywords: &["ai", "人工智能", "翻译", "translation", "model", "模型"],
            pages: vec![PageId::Translation],
        },
        Group {
            id: "proxy",
            label_key: "settings_ui.proxies.title",
            keywords: &["proxy", "代理", "socks5", "网络"],
            pages: vec![PageId::ServerManagement, PageId::Configure],
        },
        Group {
            id: "market_participants",
            label_key: "settings_ui.market_participants.title",
            keywords: &[
                "market_participants",
                "venue",
                "市场参与者",
                "场所",
                "credentials",
                "凭据",
            ],
            pages: supported_venues().into_iter().map(PageId::Venue).collect(),
        },
    ]
}

/// The pages drawn after the groups rather than inside one.
pub(crate) fn root_pages() -> Vec<PageId> {
    vec![PageId::About]
}

/// Every page, in the order the sidebar offers them.
pub(crate) fn pages() -> Vec<PageId> {
    groups()
        .into_iter()
        .flat_map(|group| group.pages)
        .chain(root_pages())
        .collect()
}

/// The page one id names, or nothing for an id no page stands for.
pub(crate) fn page(id: &str) -> Option<PageId> {
    match id {
        "appearance" => Some(PageId::Appearance),
        "data-format" => Some(PageId::DataFormat),
        "ai.translation" => Some(PageId::Translation),
        "server-management" => Some(PageId::ServerManagement),
        "configure" => Some(PageId::Configure),
        "about" => Some(PageId::About),
        _ => id
            .strip_prefix("market_participants.")
            .and_then(|name| VenueId::from_name(name).ok())
            .filter(|venue| supported_venues().contains(venue))
            .map(PageId::Venue),
    }
}

/// Whether one page is reachable through the current query.
///
/// A page is reached by its own label and keywords, or through any setting it holds: a query that
/// names a setting on a page is a query that can open that page, and the page draws only the
/// settings that matched.
pub(crate) fn page_is_reachable(page: PageId, filter: &SearchFilter, cx: &App) -> bool {
    let id = page.id();
    filter.admits_page(id.as_ref())
        || page
            .search(cx)
            .fields
            .iter()
            .any(|field| filter.admits_field(id.as_ref(), field.id.as_ref()))
}

/// The first page a filter leaves reachable, in the order the sidebar draws them.
pub(crate) fn first_admitted_page(filter: &SearchFilter, cx: &App) -> Option<PageId> {
    pages()
        .into_iter()
        .find(|page| page_is_reachable(*page, filter, cx))
}

/// The search index the sidebar and the pages are filtered by.
pub(crate) fn search_nodes(cx: &App) -> Vec<SearchNode> {
    let mut nodes = Vec::new();

    for group in groups() {
        let label = t!(cx, group.label_key);
        let mut parts = Vec::with_capacity(group.keywords.len() + 1);
        parts.push(label.as_ref());
        parts.extend(group.keywords.iter().copied());

        nodes.push(SearchNode {
            id: SharedString::new_static(group.id),
            page: None,
            keywords: vec![words(&parts)],
            children: group
                .pages
                .iter()
                .map(|page| page_node(*page, cx))
                .collect(),
        });
    }
    nodes.extend(root_pages().into_iter().map(|page| page_node(page, cx)));

    nodes
}

fn page_node(page: PageId, cx: &App) -> SearchNode {
    SearchNode {
        id: page.id(),
        page: Some(page.search(cx)),
        keywords: Vec::new(),
        children: Vec::new(),
    }
}

/// Draws one page that is a list of native setting rows.
///
/// The two views that own their layout — the translation page and the proxy catalog — are drawn
/// by the settings page itself and never reach this function.
pub(crate) fn render_page(page: PageId, args: &mut PageArgs<'_, '_>) -> AnyElement {
    match page {
        PageId::Appearance => appearance::render(args),
        PageId::DataFormat => data_format::render(args),
        PageId::About => about::render(args),
        PageId::Configure => configure::render(args),
        PageId::Venue(venue) => venues::dispatcher::render(venue, args),
        PageId::Translation | PageId::ServerManagement => {
            unreachable!("the translation and proxy pages are drawn by their own views")
        }
    }
}
