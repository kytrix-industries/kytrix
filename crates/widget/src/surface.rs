use i18n::language::codes::LanguageCode;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yLive, A11yOverride, A11yPatch},
    app::App,
    error::EngineError,
    event::Event,
    math::px,
    node::{builder::NodeBuilder, id::NodeId},
    paint::{
        style::StylePatch,
        style_builder::{PaintBuilder, StyleBuilder},
    },
    text::TextStyle,
};
use ui_kit::{kit::button::Button, theme::role::Role};

use crate::{
    cx::WidgetCx,
    status::{Failure, Notice, WidgetState, WidgetStatus},
    ui::placeholder::{self, Placeholder},
};

const BANNER_TEXT_SIZE: f32 = 12.0;
const SURFACE_RADIUS: f32 = 6.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Presentation {
    content: bool,
    placeholder: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum PlaceholderKey {
    Busy { connecting: bool },
    Notice(Notice),
    Failure(Failure),
}

pub struct WidgetSurface {
    root: NodeId,
    slot: NodeId,
    content: NodeId,
    placeholder: NodeId,
    banner: Option<NodeId>,
    status: WidgetStatus,
    language: ui_core::node::signal::Signal<LanguageCode>,
    presentation: ui_core::node::signal::Signal<Presentation>,
    last_presentation: Presentation,
    rendered_placeholder: Option<(PlaceholderKey, LanguageCode, Placeholder)>,
    rendered_banner: Option<(Failure, LanguageCode, Option<Button>)>,
}

impl WidgetSurface {
    pub fn build(
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        status: WidgetStatus,
    ) -> Result<Self, EngineError> {
        let language = cx.language();
        let presentation = cx.signal(Presentation::default());
        let theme = cx.theme().clone();

        let root_deps = status.signal();
        let root_language = language;
        let root_generation = cx.services().catalog_generation;
        let root_config = cx.services().config;
        let root = NodeBuilder::boxed()
            .id("WIDGET_SURFACE")
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .gap(px(4.0))
                    .size_full()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .bind_style(move |read| {
                let _state = read.read(&root_deps);
                let _language = read.read(&root_language);
                let _generation = read.read(&root_generation);
                let _config = read.read(&root_config);
                StylePatch::default()
            })
            .build(cx.tree_mut(), parent)?;

        let built = build_shell(cx, root, theme, presentation);
        let (slot, content, placeholder) = match built {
            Ok(shell) => shell,
            Err(error) => {
                let window = cx.window();
                let _ = cx.app_mut().remove_node(window, root);
                return Err(error);
            }
        };

        Ok(Self {
            root,
            slot,
            content,
            placeholder,
            banner: None,
            status,
            language,
            presentation,
            last_presentation: Presentation::default(),
            rendered_placeholder: None,
            rendered_banner: None,
        })
    }

    pub const fn root(&self) -> NodeId {
        self.root
    }

    pub const fn content_slot(&self) -> NodeId {
        self.content
    }

    pub const fn status(&self) -> WidgetStatus {
        self.status
    }

    pub fn sync(&mut self, cx: &mut WidgetCx<'_>, has_content: bool) -> Result<(), EngineError> {
        let state = self.status.state(cx.app());
        let language = *cx.read(&self.language);

        let next = Presentation {
            content: has_content,
            placeholder: !has_content,
        };
        if next != self.last_presentation {
            cx.write(&self.presentation, next);
            self.last_presentation = next;
        }

        self.sync_banner(cx, has_content, &state, language)?;

        if has_content {
            self.clear_placeholder(cx);
        } else {
            let key = placeholder_key(&state);
            let stale = self.rendered_placeholder.as_ref().is_none_or(
                |(rendered, rendered_language, _)| {
                    rendered != &key || *rendered_language != language
                },
            );
            if stale {
                self.clear_placeholder(cx);
                let built = build_placeholder(cx, self.placeholder, &state)?;
                self.rendered_placeholder = Some((key, language, built));
            }
            if let Some((_, _, placeholder)) = &mut self.rendered_placeholder {
                placeholder.sync(cx.app_mut())?;
            }
        }

        Ok(())
    }

    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        let banner = self
            .rendered_banner
            .as_ref()
            .and_then(|(_, _, button)| button.as_ref())
            .is_some_and(|button| button.dispatch(app, event));
        let placeholder = self
            .rendered_placeholder
            .as_ref()
            .is_some_and(|(_, _, placeholder)| placeholder.dispatch(app, event));

        banner || placeholder
    }

    pub fn release(&self, app: &mut App) -> usize {
        let banner = self
            .rendered_banner
            .as_ref()
            .and_then(|(_, _, button)| button.as_ref())
            .map_or(0, |button| button.release(app));
        let placeholder = self
            .rendered_placeholder
            .as_ref()
            .map_or(0, |(_, _, placeholder)| placeholder.release(app));

        banner + placeholder
    }

    fn sync_banner(
        &mut self,
        cx: &mut WidgetCx<'_>,
        has_content: bool,
        state: &WidgetState,
        language: LanguageCode,
    ) -> Result<(), EngineError> {
        let failure = has_content.then(|| state.failure()).flatten().cloned();

        let Some(failure) = failure else {
            self.clear_banner(cx)?;
            return Ok(());
        };

        let stale = self
            .rendered_banner
            .as_ref()
            .is_none_or(|(rendered, rendered_language, _)| {
                rendered != &failure || *rendered_language != language
            });
        if stale {
            self.clear_banner(cx)?;
            let (node, button) = build_banner(cx, self.root, &failure)?;
            cx.tree_mut()
                .append_child(self.root, self.slot)
                .map_err(|_| EngineError::StaleNode { node: self.slot })?;
            self.banner = Some(node);
            self.rendered_banner = Some((failure, language, button));
        }
        if let Some((_, _, Some(button))) = &self.rendered_banner {
            button.sync(cx.app_mut())?;
        }

        Ok(())
    }

    fn clear_banner(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let button = self
            .rendered_banner
            .take()
            .and_then(|(_, _, button)| button);
        if let Some(banner) = self.banner.take() {
            let window = cx.window();
            cx.app_mut().remove_node(window, banner)?;
        }
        if let Some(button) = button {
            let _ = button.release(cx.app_mut());
        }

        Ok(())
    }

    fn clear_placeholder(&mut self, cx: &mut WidgetCx<'_>) {
        let children: Vec<NodeId> = cx
            .tree()
            .get(self.placeholder)
            .map(|node| node.children().to_vec())
            .unwrap_or_default();
        let window = cx.window();
        for child in children {
            let _ = cx.app_mut().remove_node(window, child);
        }
        if let Some((_, _, placeholder)) = self.rendered_placeholder.take() {
            let _ = placeholder.release(cx.app_mut());
        }
    }
}

impl std::fmt::Debug for WidgetSurface {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WidgetSurface")
            .field("root", &self.root)
            .field("slot", &self.slot)
            .field("banner", &self.banner)
            .finish_non_exhaustive()
    }
}

fn placeholder_key(state: &WidgetState) -> PlaceholderKey {
    match state {
        WidgetState::Failed(failure) => PlaceholderKey::Failure(failure.clone()),
        WidgetState::Waiting(notice) => PlaceholderKey::Notice(notice.clone()),
        WidgetState::Connecting => PlaceholderKey::Busy { connecting: true },
        WidgetState::Loading | WidgetState::Ready => PlaceholderKey::Busy { connecting: false },
    }
}

fn build_placeholder(
    cx: &mut WidgetCx<'_>,
    parent: NodeId,
    state: &WidgetState,
) -> Result<Placeholder, EngineError> {
    match state {
        WidgetState::Failed(failure) => Placeholder::failure(cx, parent, failure),
        WidgetState::Waiting(notice) => Placeholder::notice(cx, parent, notice),
        WidgetState::Connecting => Placeholder::busy(
            cx,
            parent,
            Some(placeholder::busy_label(cx.translate(), true)),
        ),
        WidgetState::Loading | WidgetState::Ready => Placeholder::busy(cx, parent, None),
    }
}

fn build_shell(
    cx: &mut WidgetCx<'_>,
    root: NodeId,
    theme: ui_kit::theme::handle::ThemeHandle,
    presentation: ui_core::node::signal::Signal<Presentation>,
) -> Result<(NodeId, NodeId, NodeId), EngineError> {
    let slot_theme = theme;
    let slot_placeholder = presentation;
    let slot_content = presentation;
    let slot = NodeBuilder::boxed()
        .id("WIDGET_SLOT")
        .style(
            StyleBuilder::new()
                .flex_1()
                .min_width_0()
                .min_height_0()
                .relative()
                .overflow_hidden()
                .build(),
        )
        .paint(PaintBuilder::new().rounded(px(SURFACE_RADIUS)).build())
        .bind_style(move |read| StylePatch {
            background: Some(slot_theme.role(read, Role::SurfaceSunken)),
            border: Some(ui_core::paint::primitive::Border {
                width: px(1.0),
                color: slot_theme.role(read, Role::LineDivider),
            }),
            ..StylePatch::default()
        })
        .build(cx.tree_mut(), root)?;

    let content = NodeBuilder::boxed()
        .id("WIDGET_CONTENT")
        .style(
            StyleBuilder::new()
                .flex_col()
                .flex_1()
                .min_width_0()
                .min_height_0()
                .build(),
        )
        .bind_style(move |read| {
            let presentation = *read.read(&slot_content);
            StylePatch {
                visible: Some(presentation.content),
                ..StylePatch::default()
            }
        })
        .bind_a11y(move |read| {
            let presentation = *read.read(&slot_content);
            A11yPatch {
                hidden: Some(!presentation.content),
                ..A11yPatch::default()
            }
        })
        .build(cx.tree_mut(), slot)?;

    let placeholder = NodeBuilder::boxed()
        .id("WIDGET_PLACEHOLDER")
        .style(
            StyleBuilder::new()
                .absolute()
                .top(px(0.0))
                .right(px(0.0))
                .bottom(px(0.0))
                .left(px(0.0))
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .padding(px(16.0))
                .build(),
        )
        .paint(PaintBuilder::new().rounded(px(SURFACE_RADIUS)).build())
        .bind_style(move |read| {
            let presentation = *read.read(&slot_placeholder);
            StylePatch {
                visible: Some(presentation.placeholder),
                ..StylePatch::default()
            }
        })
        .bind_a11y(move |read| {
            let presentation = *read.read(&slot_placeholder);
            A11yPatch {
                hidden: Some(!presentation.placeholder),
                ..A11yPatch::default()
            }
        })
        .build(cx.tree_mut(), slot)?;

    Ok((slot, content, placeholder))
}

fn build_banner(
    cx: &mut WidgetCx<'_>,
    root: NodeId,
    failure: &Failure,
) -> Result<(NodeId, Option<Button>), EngineError> {
    let theme = cx.theme().clone();
    let banner = NodeBuilder::boxed()
        .id("WIDGET_BANNER")
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .padding(px(4.0))
                .width_full()
                .flex_none()
                .min_width_0()
                .build(),
        )
        .paint(PaintBuilder::new().rounded(px(SURFACE_RADIUS)).build())
        .bind_style(move |read| StylePatch {
            background: Some(theme.role(read, Role::StatusDangerSurface)),
            foreground: Some(theme.role(read, Role::StatusDangerBase)),
            border: Some(ui_core::paint::primitive::Border {
                width: px(1.0),
                color: theme.role(read, Role::StatusDangerBorder),
            }),
            ..StylePatch::default()
        })
        .a11y(A11yOverride {
            live: Some(A11yLive::Assertive),
            ..A11yOverride::default()
        })
        .build(cx.tree_mut(), root)?;

    NodeBuilder::text(SmolStr::default(), TextStyle::new(px(BANNER_TEXT_SIZE)))
        .bind_text({
            let failure = failure.clone();
            let translate = cx.translate().clone();
            let language = cx.language();
            move |read| {
                read.read(&language);
                failure.text(&translate)
            }
        })
        .build(cx.tree_mut(), banner)?;

    let button = placeholder::recovery_button(
        cx,
        banner,
        failure,
        format_smolstr!("widget.banner.action.{}", banner.to_bits()),
    )?;

    Ok((banner, button))
}

#[cfg(test)]
mod tests {
    use std::{rc::Rc, sync::Arc};

    use i18n::{language::codes::LanguageCode, translate::Translate};
    use market_participants_model::identifier::VenueId;

    use crate::{
        status::{Notice, WidgetAction, WidgetState},
        test_support::{FixtureFailure, WidgetTestApp},
    };

    fn failure() -> WidgetState {
        WidgetState::failed(
            Arc::new(FixtureFailure),
            WidgetAction::new("common.retry", |_| {}),
        )
    }

    fn product_translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    #[test]
    fn placeholder_key_distinguishes_the_three_stand_ins() {
        let failed = failure();
        let notice = WidgetState::waiting(Notice::message("test.waiting"));

        assert_ne!(
            super::placeholder_key(&WidgetState::Connecting),
            super::placeholder_key(&WidgetState::Loading),
            "connecting and loading draw different labels"
        );
        assert_ne!(
            super::placeholder_key(&notice),
            super::placeholder_key(&failed),
        );
        assert_eq!(
            super::placeholder_key(&WidgetState::Connecting),
            super::placeholder_key(&WidgetState::Connecting),
        );
    }

    #[test]
    fn the_six_rows_of_the_presentation_table() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();

        harness.report(failure());
        harness.sync_surface(&mut surface, true);
        let banner = surface.banner.expect("the banner is mounted");
        assert_eq!(
            harness.tree().get(surface.root()).expect("root").children()[0],
            banner,
            "the banner sits above the slot"
        );
        assert!(harness.visible(surface.content_slot()));
        assert!(!harness.visible(surface.placeholder));

        harness.report(WidgetState::Ready);
        harness.sync_surface(&mut surface, true);
        assert!(surface.banner.is_none(), "no failure, no banner");
        assert!(harness.visible(surface.content_slot()));
        assert!(!harness.visible(surface.placeholder));

        harness.report(failure());
        harness.sync_surface(&mut surface, false);
        assert!(surface.banner.is_none());
        assert!(!harness.visible(surface.content_slot()));
        assert!(harness.visible(surface.placeholder));
        assert!(harness.has_text(surface.placeholder, "the fixture failed"));

        harness.report(WidgetState::waiting(Notice::message("test.waiting")));
        harness.sync_surface(&mut surface, false);
        assert!(
            harness
                .text_of(surface.placeholder)
                .contains("Waiting for orders")
        );

        harness.report(WidgetState::Connecting);
        harness.sync_surface(&mut surface, false);
        assert!(harness.has_canvas(surface.placeholder));
        assert!(harness.text_of(surface.placeholder).contains("Connecting"));

        harness.report(WidgetState::Loading);
        harness.sync_surface(&mut surface, false);
        assert!(harness.has_canvas(surface.placeholder));
        assert_eq!(harness.text_of(surface.placeholder), "");

        harness.report(WidgetState::Ready);
        harness.sync_surface(&mut surface, false);
        assert!(harness.has_canvas(surface.placeholder));
        assert_eq!(harness.text_of(surface.placeholder), "");
    }

    #[test]
    fn a_waiting_placeholder_should_come_back_in_the_language_showing_now() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();
        harness.report(WidgetState::waiting(Notice::message("test.waiting")));
        harness.sync_surface(&mut surface, false);
        assert!(
            harness
                .text_of(surface.placeholder)
                .contains("Waiting for orders")
        );

        harness.switch_language(LanguageCode::ZhCn);
        harness.sync_surface(&mut surface, false);

        assert!(
            harness
                .text_of(surface.placeholder)
                .contains("正在等待订单"),
            "the stand-in is resolved in the language showing now: {}",
            harness.text_of(surface.placeholder)
        );
    }

    #[test]
    fn a_failure_banner_should_read_in_the_language_showing_now() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();
        harness.report(failure());
        harness.sync_surface(&mut surface, true);
        let banner = surface.banner.expect("the banner is mounted");
        assert!(harness.has_text(banner, "the fixture failed"));

        harness.switch_language(LanguageCode::ZhCn);
        harness.sync_surface(&mut surface, true);

        let banner = surface.banner.expect("the rebuilt banner");
        assert!(harness.has_text(banner, "示例失败"));
        assert!(
            harness.has_text(banner, "重试"),
            "the control reads in the new language: {}",
            harness.text_of(banner)
        );
    }

    #[test]
    fn a_recovery_should_be_clickable_from_the_placeholder() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();
        let retries = Rc::new(std::cell::Cell::new(0));
        let counted = retries.clone();
        harness.report(WidgetState::failed(
            Arc::new(FixtureFailure),
            WidgetAction::new("common.retry", move |_| counted.set(counted.get() + 1)),
        ));
        harness.sync_surface(&mut surface, false);

        harness.click_recovery(surface.placeholder);

        assert_eq!(retries.get(), 1);
    }

    #[test]
    fn a_disabled_recovery_should_not_run() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();
        let retries = Rc::new(std::cell::Cell::new(0));
        let counted = retries.clone();
        harness.report(WidgetState::failed(
            Arc::new(FixtureFailure),
            WidgetAction::new("common.retry", move |_| counted.set(counted.get() + 1))
                .enabled(false),
        ));
        harness.sync_surface(&mut surface, false);

        let button = harness
            .find_by_debug_prefix(surface.placeholder, "widget.placeholder.action.")
            .expect("the control is drawn");
        let key = harness
            .tree()
            .debug_id(button)
            .expect("the control carries its id")
            .trim_end_matches(":interactive")
            .to_owned();
        let action = ui_core::event::Action::Custom(smol_str::format_smolstr!("{key}.click"));
        assert!(!harness.app_mut().has_action_handler(&action));
        assert_eq!(retries.get(), 0);
    }

    #[test]
    fn a_widget_with_content_and_no_failure_shows_only_its_body() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();
        let body = harness.build_body(surface.content_slot());

        harness.sync_surface(&mut surface, true);

        assert!(harness.visible(body));
        assert!(!harness.visible(surface.placeholder));
        assert!(surface.banner.is_none());
    }

    #[test]
    fn a_steady_surface_requests_no_frame() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();
        harness.report(WidgetState::Ready);
        harness.sync_surface(&mut surface, false);
        let requests = harness.app().frame_requests();

        harness.sync_surface(&mut surface, false);

        assert_eq!(
            harness.app().frame_requests(),
            requests,
            "a steady surface scheduled no frame"
        );
    }

    #[test]
    fn writing_the_language_signal_requests_the_redrawing_frame() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();
        harness.report(WidgetState::Ready);
        harness.sync_surface(&mut surface, false);
        let requests = harness.app().frame_requests();
        let language = harness.services().language;

        assert!(harness.app_mut().write(&language, LanguageCode::ZhCn));

        assert!(
            harness.app().frame_requests() > requests,
            "the surface read the language signal while rendering, so writing it \
             requests a frame (no state re-report involved)"
        );
    }

    #[test]
    fn writing_the_directory_generation_requests_the_redrawing_frame() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();
        harness.report(WidgetState::waiting(Notice::message("test.waiting")));
        harness.sync_surface(&mut surface, false);
        let requests = harness.app().frame_requests();
        let generation = harness.services().catalog_generation;

        assert!(harness.app_mut().write(&generation, 1));

        assert!(
            harness.app().frame_requests() > requests,
            "the surface root read the generation signal while rendering, so the write \
             requests a frame (no widget body involved)"
        );
    }

    #[test]
    fn reporting_a_state_requests_the_presenting_frame() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();
        harness.report(WidgetState::Ready);
        harness.sync_surface(&mut surface, false);
        let requests = harness.app().frame_requests();

        harness.report(WidgetState::Loading);

        assert!(
            harness.app().frame_requests() > requests,
            "the surface reads the state cell while rendering, so a report requests a frame"
        );
    }

    #[test]
    fn writing_the_settings_signal_requests_the_redrawing_frame() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();
        harness.report(WidgetState::waiting(Notice::message("test.waiting")));
        harness.sync_surface(&mut surface, false);
        let requests = harness.app().frame_requests();
        let config = harness.services().config;
        let mut moved = (*harness.app().read(&config)).clone();
        moved.data_time_zone =
            app_config::time_zone::DataTimeZone::from_hours(8).expect("UTC+8 is offered");

        assert!(harness.app_mut().write(&config, moved));

        assert!(
            harness.app().frame_requests() > requests,
            "the surface root read the settings signal while rendering, so the write \
             requests a frame (no widget body involved)"
        );
    }

    #[test]
    fn the_swapped_out_subtree_is_hidden_from_assistive_technology() {
        let mut harness = WidgetTestApp::new();
        harness.enable_a11y_capture();
        let mut surface = harness.build_surface();
        harness.report(WidgetState::Ready);
        harness.sync_surface(&mut surface, false);

        assert!(
            harness.a11y_hidden(surface.content_slot()),
            "no content: the content slot is hidden in the a11y tree"
        );
        assert!(
            !harness.a11y_hidden(surface.placeholder),
            "no content: the placeholder is announced"
        );

        harness.sync_surface(&mut surface, true);

        assert!(
            !harness.a11y_hidden(surface.content_slot()),
            "content: the content slot is announced"
        );
        assert!(
            harness.a11y_hidden(surface.placeholder),
            "content: the placeholder is hidden in the a11y tree"
        );
    }

    #[test]
    fn a_rich_notice_is_drawn_as_rich_text() {
        let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
        let mut surface = harness.build_surface();
        harness.report(WidgetState::waiting(Notice::MarketNotLoaded {
            venues: vec![VenueId::Binance],
            supported: vec![VenueId::Binance, VenueId::Okx],
        }));

        harness.sync_surface(&mut surface, false);

        let text = harness.text_of(surface.placeholder);
        assert!(
            harness
                .find_by_debug_prefix(surface.placeholder, "MARKDOWN")
                .is_some(),
            "the rich notice goes through the markdown component"
        );
        assert!(
            !text.contains("<kbd>") && !text.contains("<p ") && !text.contains("![]("),
            "the notice's source markup is not drawn as text: {text:?}"
        );
        assert!(
            text.contains("Binance") && text.contains("OKX"),
            "the venues are listed by name: {text:?}"
        );
        assert!(
            harness.has_inline_boxes(surface.placeholder),
            "the kbd icon and the venue logos are inline boxes, not source text"
        );
    }

    #[test]
    fn a_rich_message_notice_is_drawn_as_rich_text() {
        for (key, has_inline_box) in [
            ("widget.news.logged_out", false),
            ("widget.empty.hint", true),
        ] {
            let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
            let mut surface = harness.build_surface();
            harness.report(WidgetState::waiting(Notice::message(key)));

            harness.sync_surface(&mut surface, false);

            let text = harness.text_of(surface.placeholder);
            assert!(
                harness
                    .find_by_debug_prefix(surface.placeholder, "MARKDOWN")
                    .is_some(),
                "{key} is authored as rich text and goes through the markdown component"
            );
            assert!(
                !text.contains("<p ") && !text.contains("<kbd>") && !text.contains("![]("),
                "{key}'s source markup is not drawn as text: {text:?}"
            );
            if has_inline_box {
                assert!(
                    harness.has_inline_boxes(surface.placeholder),
                    "{key}'s kbd icon is an inline box, not source text"
                );
            }
        }
    }
}
