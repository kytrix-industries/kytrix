//! The contract every settings page is built and driven through.
//!
//! A page is a kit-shaped component: it is created once (it owns the kit components it builds and
//! the state it keeps across frames), built into the content node the settings page hands it, and
//! then driven per frame. The engine has no entity system and no component trait, so this module is
//! the page's own shape of that contract — the shell calls it, exactly the way the shell calls a
//! kit component's `sync`/`dispatch`/`release`.
//!
//! # Lifecycle
//!
//! ```text
//! SettingsPage::open(page id)  →  Box<dyn PageView>  (constructor only; no nodes)
//!   build(app, window, parent)  →  nodes under the page's content column
//!   sync(app)                   →  controls follow the settings of the frame
//!   dispatch(app, event)        →  controls consume keys / IME / pointer
//!   release(app)                →  kit components released (nodes still alive)
//! ```
//!
//! `release` runs before the nodes are removed (the engine's teardown order: remove the subtree,
//! then release the scope). A page that is reopened is a new value, so nothing survives a page
//! switch except the settings themselves.

use ui_core::{app::App, error::EngineError, event::Event, node::id::NodeId, window::WindowId};

use crate::search::SearchFilter;

/// What a page is built with: the injected collaborators, the query it is drawn under and the key
/// prefix every kit component it builds names itself by.
///
/// The key prefix is what makes two settings pages in one application (two panes) distinct: kit
/// keys are application-level action identities, so a constant key in two panes would make one
/// pane's control dispatch the other pane's handler.
pub(crate) struct PageBuild<'a> {
    /// The answer the page filters its rows by.
    pub filter: &'a SearchFilter,
    /// The unique key prefix of the settings page instance this page belongs to.
    pub key: &'a str,
    /// The id this page is keyed under, as [PageView::page_id] states it.
    pub page: smol_str::SmolStr,
}

impl PageBuild<'_> {
    /// The application-level key one control of this page is named by.
    ///
    /// A kit key is an application-level action identity, so the same suffix in two panes would
    /// make one pane's control dispatch the other pane's handler: every key carries the settings
    /// page instance's prefix and the page it belongs to.
    pub(crate) fn unique_key(&self, suffix: &str) -> smol_str::SmolStr {
        smol_str::format_smolstr!("{}.{}.{suffix}", self.key, self.page)
    }
}

/// One page of the settings.
pub(crate) trait PageView {
    /// The id this page is keyed under.
    fn page_id(&self) -> &'static str;

    /// Builds the page's nodes under `parent`, which is the settings page's content column.
    ///
    /// # Errors
    /// Engine node-building errors (`UnknownWindow` / `StaleNode` / resource failures), or a
    /// failure from a kit component the page builds.
    fn build(
        &mut self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        cx: &PageBuild<'_>,
    ) -> Result<(), EngineError>;

    /// Makes the controls show the settings of this frame.
    ///
    /// # Errors
    /// A kit component refused a write (a `StaleNode`, or a component whose nodes were removed).
    fn sync(&mut self, app: &mut App) -> Result<(), EngineError>;

    /// Offers one platform event to the page's controls; returns whether one consumed it.
    fn dispatch(&mut self, app: &mut App, event: &Event) -> bool;

    /// Releases the kit components the page owns. The page's nodes are still alive here.
    fn release(&mut self, app: &mut App);

    /// Whether the shell wraps this page in a scroll region. A page that owns its own scrolling
    /// (the proxy catalog, the translation page) says no.
    fn scrolls(&self) -> bool {
        true
    }
}
