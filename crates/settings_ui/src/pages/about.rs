//! The about page: the version of the application that is running.

use gpui_kit::{AnyElement, App, IntoElement, ParentElement as _, SharedString, Styled as _, div};
use i18n::t;

use crate::{
    layout::{self, PageArgs},
    search::{FieldSearch, PageSearch, words},
};

/// The id of this page's setting.
const PAGE: &str = "about";

pub(crate) fn render(args: &mut PageArgs<'_, '_>) -> AnyElement {
    if !args.filter.admits_field(PAGE, "about.version") {
        return layout::page(Vec::new());
    }

    let label = t!(args.cx, "settings_ui.about.version");
    let version = SharedString::new_static(env!("CARGO_PKG_VERSION"));
    let value = div()
        .w_full()
        .text_right()
        .child(version)
        .into_any_element();

    let row = layout::row(
        "about.version",
        label,
        SharedString::default(),
        value,
        layout::CONTROL_WIDTH,
        args.cx,
    );

    layout::page(vec![layout::section(None, "about_0", vec![row], args.cx)])
}

pub(crate) fn search(cx: &App) -> PageSearch {
    let title = t!(cx, "settings_ui.about.title");
    let version = t!(cx, "settings_ui.about.version");

    PageSearch {
        label: words(&[title.as_ref(), "about", "version", "关于", "版本"]),
        fields: vec![FieldSearch {
            id: "about.version".into(),
            text: words(&[version.as_ref(), "about", "version", "关于", "版本"]),
        }],
    }
}
