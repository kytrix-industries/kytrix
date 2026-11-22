//! The about page: the version of the application that is running.

use base_ui::{style_builder::StyleBuilder, theme::role::Role};
use i18n::translate::Translate;
use smol_str::SmolStr;
use ui_core::{app::App, error::EngineError, event::Event, node::id::NodeId, window::WindowId};

use crate::{
    host::SettingsHost,
    layout,
    pages::page::{PageBuild, PageView},
    search::{FieldSearch, PageSearch, words},
};

/// The id of this page's setting.
const PAGE: &str = "about";

/// The about page.
pub(crate) struct AboutPage {
    host: SettingsHost,
}

impl AboutPage {
    /// The page over the injected collaborators, keyed by the settings page instance it belongs to.
    pub(crate) fn new(host: SettingsHost, _key: &str) -> Self {
        Self { host }
    }
}

impl PageView for AboutPage {
    fn page_id(&self) -> &'static str {
        PAGE
    }

    fn build(
        &mut self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        cx: &PageBuild<'_>,
    ) -> Result<(), EngineError> {
        if !cx.filter.admits_field(PAGE, "about.version") {
            return Ok(());
        }

        let theme = self.host.theme_handle();
        let translate = self.host.translate().clone();
        let label = i18n::t_smol!(translate, "settings_ui.about.version");
        let version = SmolStr::new_static(env!("CARGO_PKG_VERSION"));

        let row_theme = theme.clone();
        let row = layout::row(
            &theme,
            "about.version",
            label,
            SmolStr::default(),
            move |app, window, parent| {
                let value = layout::container(
                    app,
                    window,
                    parent,
                    StyleBuilder::new()
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .width_full()
                        .min_width_0()
                        .build(),
                )?;
                layout::themed_text(
                    app,
                    window,
                    value,
                    Some(SmolStr::new_static("SETTINGS_VALUE_about.version")),
                    version,
                    13.0,
                    400,
                    Role::FgDefault,
                    &row_theme,
                )?;
                Ok(())
            },
        );

        layout::section(app, window, parent, &theme, "about_0", None, vec![row])
    }

    fn sync(&mut self, _app: &mut App) -> Result<(), EngineError> {
        Ok(())
    }

    fn dispatch(&mut self, _app: &mut App, _event: &Event) -> bool {
        false
    }

    fn release(&mut self, _app: &mut App) {}
}

/// The text this page and its settings are searched by.
pub(crate) fn search(translate: &Translate) -> PageSearch {
    let title = i18n::t_smol!(translate, "settings_ui.about.title");
    let version = i18n::t_smol!(translate, "settings_ui.about.version");

    PageSearch {
        label: words(&[title.as_ref(), "about", "version", "关于", "版本"]),
        fields: vec![FieldSearch {
            id: SmolStr::new_static("about.version"),
            text: words(&[version.as_ref(), "about", "version", "关于", "版本"]),
        }],
    }
}
