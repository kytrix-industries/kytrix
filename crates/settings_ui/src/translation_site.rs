//! The dialog that chooses the site a translation model is downloaded from.
//!
//! It holds nothing but the choice: what the user confirms is handed to the callback the page
//! opened it with, and the dialog then takes itself off the layer. The page is the one that knows
//! which model and directory the choice is for, so it is also the one that starts the download.

use std::rc::Rc;

use app_config::translation::DownloadSite;
use base_ui::{
    button::Button,
    interactive::InteractiveVariantExt,
    separator::Separator,
    size::{UnifiedSizable, UnifiedSize},
    toggle::Toggle,
    tooltip::TooltipExt,
};
use gpui_kit::{
    App, Context, DismissEvent, EventEmitter, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};
use i18n::t;

/// How wide the dialog is.
const WIDTH: f32 = 420.0;

/// What one site is called in the interface.
fn site_label(site: DownloadSite, cx: &App) -> SharedString {
    match site {
        DownloadSite::HuggingFace => t!(cx, "settings_ui.ai.download_site_huggingface"),
        DownloadSite::ModelScope => t!(cx, "settings_ui.ai.download_site_modelscope"),
    }
}

/// What one site's radio is drawn under, which is what a test chooses it by.
fn site_selector(site: DownloadSite) -> &'static str {
    match site {
        DownloadSite::HuggingFace => "SETTINGS_AI_SITE_HUGGINGFACE",
        DownloadSite::ModelScope => "SETTINGS_AI_SITE_MODELSCOPE",
    }
}

/// The modal that asks which site a model download should come from.
pub(crate) struct DownloadSiteDialog {
    selected: DownloadSite,
    /// What the page does with the confirmed site, before the dialog closes.
    on_confirm: Rc<dyn Fn(DownloadSite, &mut Window, &mut App)>,
}

impl DownloadSiteDialog {
    /// A dialog that starts on `initial` and hands the confirmed site to `on_confirm`.
    pub(crate) fn new(
        initial: DownloadSite,
        on_confirm: impl Fn(DownloadSite, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            selected: initial,
            on_confirm: Rc::new(on_confirm),
        }
    }
}

impl EventEmitter<DismissEvent> for DownloadSiteDialog {}

impl Render for DownloadSiteDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dialog = cx.entity().downgrade();
        let choices = DownloadSite::ALL.into_iter().fold(
            div().w_full().flex().flex_col().gap_1(),
            |group, site| {
                let handle = dialog.clone();
                group.child(
                    Toggle::new(site_selector(site))
                        .radio()
                        .checked(self.selected == site)
                        .label(site_label(site, cx))
                        .debug_selector(move || site_selector(site).into())
                        .on_change(move |checked, _, cx| {
                            if !checked {
                                return;
                            }
                            let Some(dialog) = handle.upgrade() else {
                                return;
                            };
                            dialog.update(cx, |dialog, cx| {
                                dialog.selected = site;
                                cx.notify();
                            });
                        }),
                )
            },
        );

        div()
            .debug_selector(|| "SETTINGS_AI_SITE_DIALOG".into())
            .w(px(WIDTH))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_base()
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .child(t!(cx, "settings_ui.ai.download_site_title")),
            )
            .child(choices)
            .child(
                div()
                    .w_full()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(Separator::horizontal())
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("settings-ai-site-cancel")
                                    .ghost()
                                    .with_unified_size(UnifiedSize::Medium)
                                    .debug_selector(|| "SETTINGS_AI_SITE_CANCEL".into())
                                    .aria_label(t!(cx, "common.cancel"))
                                    .with_tooltip(t!(cx, "common.cancel"))
                                    .child(t!(cx, "common.cancel"))
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                            )
                            .child(
                                Button::new("settings-ai-site-confirm")
                                    .filled()
                                    .with_unified_size(UnifiedSize::Medium)
                                    .debug_selector(|| "SETTINGS_AI_SITE_CONFIRM".into())
                                    .aria_label(t!(cx, "common.confirm"))
                                    .with_tooltip(t!(cx, "common.confirm"))
                                    .child(t!(cx, "common.confirm"))
                                    .on_click(cx.listener(|dialog, _, window, cx| {
                                        let confirm = Rc::clone(&dialog.on_confirm);
                                        confirm(dialog.selected, window, cx);
                                        cx.emit(DismissEvent);
                                    })),
                            ),
                    ),
            )
    }
}
