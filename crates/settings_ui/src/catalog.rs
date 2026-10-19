//! The settings a build offers: the tree, the pages behind it, and the fields on them.
//!
//! This is a declaration rather than a database. Nothing here reads the settings or draws
//! anything: a page says what it holds and how each control converts to and from the stored
//! value, and the dialog is what turns that into a tree and a form.
//!
//! Ids are the identity of a setting. A field id is what a value is remembered under while
//! it is being typed and what a search matches it against, so it is not a rendering detail
//! and does not change with the interface.

use std::sync::Arc;

use app_config::settings::Config;
use gpui_kit::SharedString;
use market_participants_model::identifier::VenueId;
use smol_str::SmolStr;

use crate::{
    access::{FieldGetter, FieldSetter, FieldValue, InfoGetter, ProxyLineGetter, ProxyLineSetter},
    schema,
};

/// The page the settings open on.
pub(crate) const DEFAULT_PAGE: &str = "appearance";

/// The page a dialog opens on.
pub(crate) const fn default_page() -> &'static str {
    DEFAULT_PAGE
}

/// The venues this build serves, as the settings draw a group for each.
///
/// The rows are the build's static venue list rather than a runtime view: a venue this build
/// serves has its line, its origins and its credentials whether or not anything is loaded, which
/// is what makes loading one possible at all.
pub(crate) type VenueView = Arc<[VenueId]>;

/// One label, as either the key it is looked up under or the name it is drawn with.
///
/// Keys are resolved while the page is drawn rather than stored as text, so a catalog built
/// once shows whatever language the window is showing now. A canonical identity that nobody
/// translates — the name of a scope this build speaks — is drawn as it is, the same way
/// [`PageTitle`] names a page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Text {
    /// A message key the label is looked up under.
    Key(SharedString),
    /// A name drawn as it is, such as one scope's canonical identity.
    Literal(SharedString),
}

impl Text {
    pub const fn new(key: &'static str) -> Self {
        Self::Key(SharedString::new_static(key))
    }

    /// A label drawn as it is, such as the canonical name of a scope this build speaks.
    pub fn literal(name: impl Into<SharedString>) -> Self {
        Self::Literal(name.into())
    }

    /// The message key this label is looked up under, or `None` when it is drawn as it is.
    ///
    /// Only the guards read this: what a page draws is [`Self::label`], which resolves the key in
    /// the language the window shows, and the guards are what compare the keys a catalog declares
    /// against the ones the locales define.
    #[cfg(test)]
    pub fn key(&self) -> Option<&str> {
        match self {
            Self::Key(key) => Some(key.as_str()),
            Self::Literal(_) => None,
        }
    }

    /// The words this label draws in the language the window shows.
    pub fn label(&self, cx: &gpui_kit::App) -> SharedString {
        match self {
            Self::Key(key) => i18n::t!(cx, key.as_str()),
            Self::Literal(name) => name.clone(),
        }
    }

    /// The words a user can reach this label by without knowing the text it renders as.
    ///
    /// A key is a dotted path whose last segments name the setting in English, so
    /// `settings_ui.proxies.socks5_port` is reachable by "socks5_port" and by "port"
    /// whatever the window is showing. A label rendered in another language is reached by
    /// the keywords the catalog declares beside it, and a name drawn as it is is reached by
    /// its own words.
    pub fn search_words(&self, rendered: &str) -> String {
        let mut words = String::with_capacity(rendered.len() + 8);
        words.push_str(&rendered.to_lowercase());
        words.push(' ');

        let Self::Key(key) = self else {
            return words;
        };

        let key = key.strip_prefix("settings_ui.").unwrap_or(key);
        words.push_str(&key.to_lowercase());
        words.push(' ');
        for segment in key.split('.').skip(1) {
            words.push_str(&segment.to_lowercase());
            words.push(' ');
        }

        words
    }
}

/// How one page is named.
///
/// A page built from the settings carries the identity it stands for, and an identity nobody
/// translates is drawn as it is: a venue's own name is the catalogue's, and a scope's canonical
/// name is the build's. Everything else is named by a message key, so it follows the language the
/// window shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PageTitle {
    /// A message key the page's label is looked up under.
    Key(Text),
    /// A name drawn as it is, such as one scope's canonical identity.
    Literal(SharedString),
}

impl PageTitle {
    /// The name this page is drawn with.
    pub fn label(&self, cx: &gpui_kit::App) -> SharedString {
        match self {
            Self::Key(text) => text.label(cx),
            Self::Literal(name) => name.clone(),
        }
    }

    /// The words a user can reach this page by, in the language it is drawn in.
    pub fn search_words(&self, cx: &gpui_kit::App) -> String {
        match self {
            Self::Key(text) => text.search_words(&self.label(cx)),
            Self::Literal(name) => name.to_lowercase(),
        }
    }
}

/// One choice of a [`FieldAccess::Select`].
pub(crate) struct SelectOptionDef {
    pub label: Text,
    /// The stored form of this choice, which is what the page writes.
    pub id: FieldValue,
}

impl SelectOptionDef {
    pub const fn new(label: Text, id: &'static str) -> Self {
        Self {
            label,
            id: SmolStr::new_static(id),
        }
    }

    /// A choice drawn as the name it is stored under.
    ///
    /// An identity nobody translates — a UTC offset, a scope this build speaks — is
    /// the same text in every language, so it is carried as the literal it is rather
    /// than looked up under a key.
    pub const fn literal(label: &'static str, id: &'static str) -> Self {
        Self {
            label: Text::Literal(SharedString::new_static(label)),
            id: SmolStr::new_static(id),
        }
    }
}

/// How one field is presented, and what it converts to and from the settings.
///
/// The conversions are closures because a field built from the settings carries
/// the identity it stands for: one origin row is one market of one venue and one credential
/// field is one value of one venue's credential set, so nothing has to be looked up from a field
/// id while a control is drawn or written.
#[derive(Clone)]
pub(crate) enum FieldAccess {
    /// A value the page only reports.
    Info { get: InfoGetter },
    /// A line of text, committed when the field loses the caret.
    Text {
        get: FieldGetter,
        /// Writes what the field holds. A field standing for a value nested in a structure
        /// the user may not have configured yet writes nothing in that case.
        set: FieldSetter,
        placeholder: Option<Text>,
        password: bool,
    },
    /// One of a fixed set of values.
    Select {
        options: &'static [SelectOptionDef],
        get: FieldGetter,
        set: FieldSetter,
    },
    /// One of the lines a venue or a group of Kytrix services is bound to.
    ///
    /// The choices depend on the proxy catalog, which is not a compile-time fact: the entries
    /// of the session the dialog holds are read from it while the control is drawn. Which venue
    /// or which services the field covers is a compile-time fact of the page that declares it, so
    /// it is captured by the closures rather than looked up from the field's id.
    ProxyLine {
        /// Whether the row is the document-wide default line itself: it is the one line no
        /// other line can follow, so the row that sets it offers no "follow the default line"
        /// choice.
        default_line: bool,
        get: ProxyLineGetter,
        set: ProxyLineSetter,
    },
}

impl FieldAccess {
    /// The value the field stands for now.
    pub(crate) fn current(&self, config: &Config) -> FieldValue {
        match self {
            Self::Info { .. } => FieldValue::default(),
            Self::Text { get, .. } | Self::Select { get, .. } => get(config),
            Self::ProxyLine { get, .. } => get(config),
        }
    }

    /// Writes a value the user chose.
    ///
    /// A reported field has nothing to write, and a user cannot operate one.
    pub(crate) fn write(&self, config: &mut Config, value: &FieldValue) {
        match self {
            Self::Info { .. } => {}
            Self::Text { set, .. } | Self::Select { set, .. } => set(config, value),
            Self::ProxyLine { set, .. } => set(config, value),
        }
    }

    /// The text a reported field shows, or `None` for a field the user operates.
    pub(crate) fn info(&self, config: &Config) -> Option<SharedString> {
        match self {
            Self::Info { get } => Some(get(config)),
            Self::Text { .. } | Self::Select { .. } | Self::ProxyLine { .. } => None,
        }
    }
}

/// One control on a page.
#[derive(Clone)]
pub(crate) struct FieldDef {
    pub id: SharedString,
    pub label: Text,
    pub description: Text,
    /// Extra words a search matches this field by, in every supported language.
    pub keywords: &'static [&'static str],
    pub access: FieldAccess,
}

/// A group of fields under one heading.
#[derive(Clone)]
pub(crate) struct SectionDef {
    pub title: Option<Text>,
    /// Whether a separator is drawn above the heading, which is how one section is told
    /// apart from the one before it on a long page.
    pub separated: bool,
    pub fields: Vec<FieldDef>,
}

impl SectionDef {
    pub fn fields(title: Option<Text>, separated: bool, fields: Vec<FieldDef>) -> Self {
        Self {
            title,
            separated,
            fields,
        }
    }
}

/// One page of the settings.
///
/// A page is a declaration: its identity is the id this module writes, and its fields are the
/// controls the dialog draws for it. The venue pages are built one per venue this build speaks,
/// so which of them exist is a fact of the build rather than of anything loaded or stored.
#[derive(Clone)]
pub(crate) struct PageDef {
    pub id: SharedString,
    pub title: PageTitle,
    /// The words a search matches this page by, beyond its own label.
    pub keywords: &'static [&'static str],
    pub sections: Vec<SectionDef>,
}

/// One entry of the settings tree.
pub(crate) enum TreeNodeDef {
    Page(SharedString),
    Group {
        id: &'static str,
        label: Text,
        keywords: &'static [&'static str],
        /// Whether the group is open when the dialog is drawn without a search.
        open: bool,
        children: Vec<TreeNodeDef>,
    },
}

impl TreeNodeDef {
    /// The id a selection and a search both name this entry by.
    pub fn id(&self) -> &str {
        match self {
            Self::Page(id) => id.as_str(),
            Self::Group { id, .. } => id,
        }
    }

    /// The label the tree draws for this entry.
    pub fn label<'a>(&'a self, catalog: &'a Catalog) -> PageTitle {
        match self {
            Self::Page(id) => catalog.page_label(id.as_str()),
            Self::Group { label, .. } => PageTitle::Key(label.clone()),
        }
    }
}

/// Everything the settings offer, indexed for lookup.
pub(crate) struct Catalog {
    /// The tree in the order it is drawn.
    pub tree: Vec<TreeNodeDef>,
    /// Every page, in the order the tree draws them.
    pub pages: Vec<PageDef>,
}

impl Catalog {
    /// The page drawn behind one tree entry.
    pub fn page(&self, id: &str) -> Option<&PageDef> {
        self.pages.iter().find(|page| page.id == id)
    }

    /// The id of the page behind one tree entry, or `None` when the entry is a group.
    ///
    /// It answers with the page's own id rather than with what was asked for, so the id a
    /// page is selected and compared by is always the one the catalog declares.
    pub fn page_id(&self, id: &str) -> Option<SharedString> {
        self.pages
            .iter()
            .find(|page| page.id == id)
            .map(|page| page.id.clone())
    }

    /// The label a page is drawn with, or an empty label for an id that names none.
    pub fn page_label(&self, id: &str) -> PageTitle {
        self.page(id).map_or_else(
            || PageTitle::Literal(SharedString::default()),
            |page| page.title.clone(),
        )
    }
}

/// Builds the catalog of the venues this build serves.
///
/// The pages are a function of the static venue list alone: a venue this build serves has
/// its line, its addresses and its credential fields whether or not anything is loaded,
/// because configuring them is what makes loading a venue possible at all. No part of a
/// page comes from a runtime view or from a stored value, so a catalog never has to be
/// built again while the settings move under it.
pub(crate) fn catalog(venues: &VenueView) -> Catalog {
    let (tree, pages) = schema::catalog(venues);

    Catalog { tree, pages }
}
