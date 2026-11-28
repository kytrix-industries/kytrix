use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};

use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    interaction::ScrollHandle,
    math::{LogicalPx, px},
    node::{
        builder::NodeBuilder,
        dependency::ReadCtx,
        id::NodeId,
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
    paint::{style::StylePatch, style_builder::StyleBuilder},
    text::{TextAlign, TextStyle},
    window::WindowId,
};
use ui_kit::{
    kit::{
        button::Button,
        dialog::{Dialog, DialogBuilder},
        markdown::{MarkdownAssets, MarkdownNode},
        progress::Progress,
        scroll_bar::{ScrollBar, ScrollbarAxis, ScrollbarOptions},
        separator::Separator,
        size::UnifiedSize,
        spinner::Spinner,
    },
    theme::{handle::ThemeHandle, role::Role},
};
use widget::{error::dialog_engine_error, services::WidgetServices};

use crate::{
    publish,
    updater::{DialogControl, DownloadPhase, UpdaterCore, UpdaterView, control_callback},
};

const NOTES_WIDTH: f32 = 1040.0;

const STATUS_WIDTH: f32 = 640.0;

const NOTES_MAX_HEIGHT: f32 = 640.0;

const SCROLLBAR_WIDTH: f32 = 8.0;

const CONTENT_GAP: f32 = 8.0;

const BODY_SIZE: f32 = 14.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    Notes,
    Downloading,
    Pausing,
    Paused,
    Downloaded,
    Installing,
    Failure,
}

impl Shape {
    fn of(view: UpdaterView) -> Self {
        match view {
            UpdaterView::Notes => Self::Notes,
            UpdaterView::Download { phase, .. } => match phase {
                DownloadPhase::Downloading => Self::Downloading,
                DownloadPhase::Pausing => Self::Pausing,
                DownloadPhase::Paused => Self::Paused,
            },
            UpdaterView::Downloaded => Self::Downloaded,
            UpdaterView::Installing => Self::Installing,
            UpdaterView::Failure(_) => Self::Failure,
        }
    }

    fn width(self) -> f32 {
        match self {
            Self::Notes => NOTES_WIDTH,
            _ => STATUS_WIDTH,
        }
    }
}

pub(crate) struct DialogData {
    pub view: UpdaterView,
    pub title: SmolStr,
    pub notes: Option<SmolStr>,
    pub language: LanguageCode,
    pub install_supported: bool,
}

pub(crate) struct Built {
    shape: Shape,
    language: LanguageCode,
    notes: Option<SmolStr>,
    install_supported: bool,
    markdown: Option<MarkdownNode>,
    scrollbar: Option<ScrollBar>,
    progress: Option<Progress>,
    spinner: Option<Spinner>,
    pub buttons: Vec<Button>,
}

impl Built {
    fn new(plan: &Plan) -> Self {
        Self {
            shape: plan.shape,
            language: plan.language,
            notes: plan.notes.clone(),
            install_supported: plan.install_supported,
            markdown: None,
            scrollbar: None,
            progress: None,
            spinner: None,
            buttons: Vec::new(),
        }
    }

    fn release(&mut self, app: &mut App) -> usize {
        let mut released = 0;
        if let Some(markdown) = self.markdown.as_mut() {
            markdown.release(app);
        }
        if let Some(scrollbar) = self.scrollbar.as_ref() {
            released += scrollbar.release(app);
        }
        if let Some(progress) = self.progress.as_ref() {
            released += progress.release(app);
        }
        if let Some(spinner) = self.spinner.as_ref() {
            released += spinner.release(app);
        }
        for button in &self.buttons {
            released += button.release(app);
        }
        self.markdown = None;
        self.scrollbar = None;
        self.progress = None;
        self.spinner = None;
        self.buttons.clear();
        released
    }
}

pub(crate) struct UpdateDialog {
    key: SmolStr,
    theme: ThemeHandle,
    translate: Translate,
    view: Signal<Option<UpdaterView>>,
    shown: Signal<Option<UpdaterView>>,
    language: Signal<LanguageCode>,
    updater: Weak<RefCell<UpdaterCore>>,
    scope: SignalScope,
    dialog: Option<Dialog>,
    pub built: Rc<RefCell<Option<Built>>>,
    released: bool,
}

impl UpdateDialog {
    pub(crate) fn new(
        app: &mut App,
        services: Rc<WidgetServices>,
        key: SmolStr,
        view: Signal<Option<UpdaterView>>,
        updater: Weak<RefCell<UpdaterCore>>,
    ) -> Self {
        let scope = app.signal_scope();
        let shown = app.signal_scoped(scope, None);
        Self {
            key,
            theme: services.theme.clone(),
            translate: services.translate.clone(),
            language: services.language,
            view,
            shown,
            updater,
            scope,
            dialog: None,
            built: Rc::new(RefCell::new(None)),
            released: false,
        }
    }

    pub(crate) fn show(
        &mut self,
        app: &mut App,
        window: WindowId,
        data: DialogData,
    ) -> Result<(), EngineError> {
        if self.released {
            return Ok(());
        }
        let shape = Shape::of(data.view);
        let is_current = self
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.is_open(app))
            && self.built.borrow().as_ref().is_some_and(|built| {
                built.shape == shape
                    && built.language == data.language
                    && built.notes == data.notes
                    && built.install_supported == data.install_supported
            });
        if is_current {
            return Ok(());
        }

        self.release_built(app);
        if let Some(dialog) = self.dialog.take() {
            dialog.close_now(app);
            dialog.release(app);
        }
        let _ = app.write(&self.shown, Some(data.view));

        let theme = self.theme.clone();
        let translate = self.translate.clone();
        let shown = self.shown;
        let language = self.language;
        let updater = self.updater.clone();
        let key = self.key.clone();
        let built = Rc::clone(&self.built);
        let plan = Plan {
            shape,
            notes: data.notes.clone(),
            install_supported: data.install_supported,
            language: data.language,
            body_size: UnifiedSize::Medium.text_size(),
            secondary_size: UnifiedSize::Small.text_size(),
        };
        let dialog = DialogBuilder::new(data.title)
            .header(false)
            .close_button(false)
            .width(shape.width())
            .content(move |app, window, body| {
                build_content(
                    app, window, body, &plan, &key, &theme, &translate, shown, language, &updater,
                    &built,
                )
            })
            .build(
                app,
                self.theme.clone(),
                format_smolstr!("{}.dialog", self.key),
            );
        dialog.open(app, window).map_err(dialog_engine_error)?;
        self.dialog = Some(dialog);
        Ok(())
    }

    pub(crate) fn sync(&mut self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        if self.released {
            return Ok(());
        }
        let Some(dialog) = self.dialog.as_ref() else {
            return Ok(());
        };
        if dialog.is_open(app) {
            if !dialog.is_closing(app) {
                let live = *app.read(&self.view);
                if *app.read(&self.shown) != live {
                    let _ = app.write(&self.shown, live);
                }
            }
            let view = *app.read(&self.shown);
            let mut built = self.built.borrow_mut();
            if let Some(built) = built.as_mut() {
                if let Some(progress) = built.progress.as_ref() {
                    if let Some(percent) = view.and_then(UpdaterView::percent) {
                        let _ = progress.set_value(app, percent);
                    }
                    let _ = progress.sync(app, window)?;
                }
                if let Some(spinner) = built.spinner.as_ref() {
                    let _ = spinner.sync(app)?;
                }
                if let Some(scrollbar) = built.scrollbar.as_ref() {
                    let _ = scrollbar.sync(app)?;
                }
                if let Some(markdown) = built.markdown.as_mut() {
                    let _ = markdown.sync(app);
                }
                for button in &built.buttons {
                    let _ = button.sync(app)?;
                }
            }
        }
        dialog.sync(app).map_err(dialog_engine_error)?;
        if !dialog.is_open(app) && !dialog.is_closing(app) {
            self.release_built(app);
        }
        Ok(())
    }

    pub(crate) fn close(&mut self, app: &mut App) {
        if let Some(dialog) = self.dialog.as_ref() {
            dialog.close(app);
        }
    }

    pub(crate) fn is_open(&self, app: &App) -> bool {
        self.dialog
            .as_ref()
            .is_some_and(|dialog| dialog.is_open(app))
    }

    pub(crate) fn is_closing(&self, app: &App) -> bool {
        self.dialog
            .as_ref()
            .is_some_and(|dialog| dialog.is_closing(app))
    }

    pub(crate) fn release(&mut self, app: &mut App) -> usize {
        if self.released {
            return 0;
        }
        self.released = true;
        let mut released = self.release_built(app);
        if let Some(dialog) = self.dialog.take() {
            dialog.close_now(app);
            dialog.release(app);
        }
        released += app.release_scope(self.scope);
        released
    }

    fn release_built(&mut self, app: &mut App) -> usize {
        let Some(mut built) = self.built.borrow_mut().take() else {
            return 0;
        };
        built.release(app)
    }
}

struct Plan {
    shape: Shape,
    notes: Option<SmolStr>,
    install_supported: bool,
    language: LanguageCode,
    body_size: LogicalPx,
    secondary_size: LogicalPx,
}

fn build_content(
    app: &mut App,
    window: WindowId,
    body: NodeId,
    plan: &Plan,
    key: &SmolStr,
    theme: &ThemeHandle,
    translate: &Translate,
    view: Signal<Option<UpdaterView>>,
    language: Signal<LanguageCode>,
    updater: &Weak<RefCell<UpdaterCore>>,
    built: &Rc<RefCell<Option<Built>>>,
) -> Result<(), EngineError> {
    let root = build_node(app, window, body, |tree, body| {
        NodeBuilder::boxed()
            .id(format_smolstr!("{key}:content"))
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .width_full()
                    .gap(px(CONTENT_GAP))
                    .build(),
            )
            .build(tree, body)
    })?;

    let mut widgets = Built::new(plan);
    match plan.shape {
        Shape::Notes => build_notes(
            app,
            window,
            root,
            plan,
            key,
            theme,
            translate,
            language,
            &mut widgets,
        )?,
        Shape::Downloading | Shape::Pausing | Shape::Paused => build_download(
            app,
            window,
            root,
            plan,
            key,
            theme,
            translate,
            view,
            language,
            &mut widgets,
        )?,
        Shape::Downloaded | Shape::Installing => {
            build_downloaded(app, window, root, plan, theme, translate, language)?
        }
        Shape::Failure => build_failure(app, window, root, key, theme, translate, view, language)?,
    }

    let _ = Separator::horizontal(theme.clone()).build(tree_mut(app, window)?, root)?;
    let row = build_node(app, window, root, |tree, root| {
        NodeBuilder::boxed()
            .id(format_smolstr!("{key}:actions"))
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .width_full()
                    .gap(px(CONTENT_GAP))
                    .build(),
            )
            .build(tree, root)
    })?;
    build_buttons(
        app,
        window,
        row,
        plan,
        key,
        theme,
        translate,
        updater,
        &mut widgets,
    )?;
    *built.borrow_mut() = Some(widgets);
    Ok(())
}

fn build_notes(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    plan: &Plan,
    key: &SmolStr,
    theme: &ThemeHandle,
    translate: &Translate,
    language: Signal<LanguageCode>,
    widgets: &mut Built,
) -> Result<(), EngineError> {
    let band = build_node(app, window, parent, |tree, parent| {
        NodeBuilder::boxed()
            .id(format_smolstr!("{key}:notes-band"))
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .relative()
                    .width_full()
                    .min_width_0()
                    .build(),
            )
            .build(tree, parent)
    })?;
    let viewport = build_node(app, window, band, |tree, band| {
        NodeBuilder::boxed()
            .id(format_smolstr!("{key}:notes"))
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .flex_1()
                    .min_width_0()
                    .max_height(px(NOTES_MAX_HEIGHT))
                    .padding_sides(px(0.0), px(SCROLLBAR_WIDTH), px(0.0), px(0.0))
                    .overflow_hidden()
                    .build(),
            )
            .build(tree, band)
    })?;
    let _handle: ScrollHandle = app.make_scroll(window, viewport, Default::default())?;

    let source = plan.notes.clone().unwrap_or_default();
    let markdown = MarkdownNode::mount(
        app,
        window,
        viewport,
        theme.clone(),
        &source,
        ReleaseNotesAssets,
    )?;
    widgets.markdown = Some(markdown);

    let scrollbar_band = build_node(app, window, band, |tree, band| {
        NodeBuilder::boxed()
            .id(format_smolstr!("{key}:notes-scrollbar-band"))
            .style(
                StyleBuilder::new()
                    .absolute()
                    .top(px(0.0))
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .width(px(SCROLLBAR_WIDTH))
                    .build(),
            )
            .build(tree, band)
    })?;
    let scrollbar = ScrollBar::new(
        app,
        theme.clone(),
        format_smolstr!("{key}:notes-scrollbar"),
        ScrollbarAxis::Vertical,
        ScrollbarOptions::default(),
    );
    let _ = scrollbar.build(tree_mut(app, window)?, scrollbar_band)?;
    scrollbar.attach(app, window, viewport)?;
    widgets.scrollbar = Some(scrollbar);

    if !plan.install_supported {
        let text_translate = translate.clone();
        let size = plan.secondary_size;
        build_node(app, window, parent, |tree, parent| {
            NodeBuilder::text(SmolStr::default(), TextStyle::new(size))
                .style(StyleBuilder::new().width_full().build())
                .bind_text(move |cx| {
                    let _ = *cx.read(&language);
                    t_smol!(text_translate, "window.updater.manual_required")
                })
                .build(tree, parent)
        })?;
    }
    Ok(())
}

fn build_download(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    plan: &Plan,
    key: &SmolStr,
    theme: &ThemeHandle,
    translate: &Translate,
    view: Signal<Option<UpdaterView>>,
    language: Signal<LanguageCode>,
    widgets: &mut Built,
) -> Result<(), EngineError> {
    let heading_translate = translate.clone();
    let heading_size = plan.body_size;
    build_node(app, window, parent, |tree, parent| {
        NodeBuilder::text(SmolStr::default(), {
            let mut style = TextStyle::new(heading_size);
            style.alignment = TextAlign::Center;
            style
        })
        .style(StyleBuilder::new().width_full().build())
        .bind_text(move |cx| {
            let _ = *cx.read(&language);
            match *cx.read(&view) {
                Some(UpdaterView::Download { phase, .. }) => match phase {
                    DownloadPhase::Downloading => {
                        t_smol!(heading_translate, "window.updater.downloading")
                    }
                    DownloadPhase::Pausing => t_smol!(heading_translate, "window.updater.pausing"),
                    DownloadPhase::Paused => t_smol!(heading_translate, "window.updater.paused"),
                },
                _ => SmolStr::default(),
            }
        })
        .build(tree, parent)
    })?;

    let percent = app.read(&view).and_then(UpdaterView::percent);
    if let Some(percent) = percent {
        let progress =
            Progress::new(app, theme.clone(), format_smolstr!("{key}:progress")).value(percent);
        let _ = progress.build(app, window, parent)?;
        let percent_translate = translate.clone();
        let percent_size = plan.secondary_size;
        build_node(app, window, parent, |tree, parent| {
            NodeBuilder::text(SmolStr::default(), {
                let mut style = TextStyle::new(percent_size);
                style.alignment = TextAlign::Center;
                style
            })
            .style(StyleBuilder::new().width_full().build())
            .bind_text(move |cx| {
                let _ = *cx.read(&language);
                match (*cx.read(&view)).and_then(UpdaterView::percent) {
                    Some(percent) => t_smol!(
                        percent_translate,
                        "window.updater.download_progress",
                        percent = percent.round() as u32
                    ),
                    None => SmolStr::default(),
                }
            })
            .build(tree, parent)
        })?;
        widgets.progress = Some(progress);
    } else {
        let spinner_parent = build_node(app, window, parent, |tree, parent| {
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:spinner-row"))
                .style(
                    StyleBuilder::new()
                        .flex_row()
                        .items_center()
                        .justify_center()
                        .width_full()
                        .build(),
                )
                .build(tree, parent)
        })?;
        let spinner = Spinner::new(app, theme.clone(), format_smolstr!("{key}:spinner"))
            .with_role(Role::BrandBase)
            .with_size(plan.body_size);
        spinner.build(app, window, spinner_parent)?;
        widgets.spinner = Some(spinner);
    }
    Ok(())
}

fn build_downloaded(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    plan: &Plan,
    theme: &ThemeHandle,
    translate: &Translate,
    language: Signal<LanguageCode>,
) -> Result<(), EngineError> {
    let message_translate = translate.clone();
    let message_size = plan.body_size;
    build_text(app, window, parent, message_size, move |cx| {
        let _ = *cx.read(&language);
        t_smol!(message_translate, "window.updater.downloaded_message")
    })?;
    let hint_translate = translate.clone();
    let hint_theme = theme.clone();
    let hint_size = plan.secondary_size;
    build_text(app, window, parent, hint_size, move |cx| {
        let _ = *cx.read(&language);
        let _ = hint_theme.role(cx, Role::FgMuted);
        t_smol!(hint_translate, "window.updater.restart_hint")
    })?;
    Ok(())
}

fn build_failure(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    key: &SmolStr,
    theme: &ThemeHandle,
    translate: &Translate,
    view: Signal<Option<UpdaterView>>,
    language: Signal<LanguageCode>,
) -> Result<(), EngineError> {
    let text_theme = theme.clone();
    let text_translate = translate.clone();
    build_node(app, window, parent, |tree, parent| {
        NodeBuilder::text(SmolStr::default(), TextStyle::new(px(BODY_SIZE)))
            .id(format_smolstr!("{key}:failure"))
            .style(StyleBuilder::new().width_full().build())
            .bind_text(move |cx| {
                let _ = *cx.read(&language);
                match *cx.read(&view) {
                    Some(UpdaterView::Failure(key)) => t_smol!(text_translate, key),
                    _ => SmolStr::default(),
                }
            })
            .bind_style(move |cx| StylePatch {
                foreground: Some(text_theme.role(cx, Role::StatusDangerBase)),
                ..StylePatch::default()
            })
            .build(tree, parent)
    })?;
    Ok(())
}

fn build_text(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    size: LogicalPx,
    bind: impl Fn(&mut ReadCtx<'_>) -> SmolStr + 'static,
) -> Result<NodeId, EngineError> {
    build_node(app, window, parent, |tree, parent| {
        NodeBuilder::text(SmolStr::default(), {
            let mut style = TextStyle::new(size);
            style.alignment = TextAlign::Center;
            style
        })
        .style(StyleBuilder::new().width_full().build())
        .bind_text(bind)
        .build(tree, parent)
    })
}

fn build_buttons(
    app: &mut App,
    window: WindowId,
    row: NodeId,
    plan: &Plan,
    key: &SmolStr,
    theme: &ThemeHandle,
    translate: &Translate,
    updater: &Weak<RefCell<UpdaterCore>>,
    widgets: &mut Built,
) -> Result<(), EngineError> {
    let cancel = t_smol!(translate, "window.updater.cancel");
    let close = t_smol!(translate, "common.close");
    let mut buttons = Vec::new();
    match plan.shape {
        Shape::Notes => {
            buttons.push(control_button(
                app,
                window,
                row,
                key,
                "cancel",
                theme,
                cancel,
                DialogControl::Close,
                ButtonVariant::Secondary,
                false,
                updater,
            )?);
            if plan.install_supported {
                buttons.push(control_button(
                    app,
                    window,
                    row,
                    key,
                    "download",
                    theme,
                    t_smol!(translate, "window.updater.download"),
                    DialogControl::Download,
                    ButtonVariant::Primary,
                    false,
                    updater,
                )?);
            } else {
                buttons.push(release_page_button(
                    app, window, row, key, theme, translate, updater,
                )?);
            }
        }
        Shape::Downloading => {
            buttons.push(cancel_button(
                app, window, row, key, theme, &cancel, updater,
            )?);
            buttons.push(control_button(
                app,
                window,
                row,
                key,
                "pause",
                theme,
                t_smol!(translate, "window.updater.pause"),
                DialogControl::Pause,
                ButtonVariant::Secondary,
                false,
                updater,
            )?);
        }
        Shape::Pausing => {
            buttons.push(cancel_button(
                app, window, row, key, theme, &cancel, updater,
            )?);
            buttons.push(control_button(
                app,
                window,
                row,
                key,
                "pausing",
                theme,
                t_smol!(translate, "window.updater.pausing"),
                DialogControl::Pause,
                ButtonVariant::Secondary,
                true,
                updater,
            )?);
        }
        Shape::Paused => {
            buttons.push(cancel_button(
                app, window, row, key, theme, &cancel, updater,
            )?);
            buttons.push(control_button(
                app,
                window,
                row,
                key,
                "resume",
                theme,
                t_smol!(translate, "window.updater.resume"),
                DialogControl::Resume,
                ButtonVariant::Secondary,
                false,
                updater,
            )?);
        }
        Shape::Downloaded => {
            buttons.push(control_button(
                app,
                window,
                row,
                key,
                "cancel",
                theme,
                cancel,
                DialogControl::Discard,
                ButtonVariant::Secondary,
                false,
                updater,
            )?);
            buttons.push(control_button(
                app,
                window,
                row,
                key,
                "install",
                theme,
                t_smol!(translate, "window.updater.install"),
                DialogControl::Install,
                ButtonVariant::Primary,
                false,
                updater,
            )?);
        }
        Shape::Installing => {}
        Shape::Failure => {
            buttons.push(control_button(
                app,
                window,
                row,
                key,
                "dismiss",
                theme,
                close,
                DialogControl::DismissFailure,
                ButtonVariant::Secondary,
                false,
                updater,
            )?);
        }
    }
    widgets.buttons = buttons;
    Ok(())
}

fn cancel_button(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    key: &SmolStr,
    theme: &ThemeHandle,
    label: &SmolStr,
    updater: &Weak<RefCell<UpdaterCore>>,
) -> Result<Button, EngineError> {
    control_button(
        app,
        window,
        parent,
        key,
        "cancel",
        theme,
        label.clone(),
        DialogControl::Cancel,
        ButtonVariant::Secondary,
        false,
        updater,
    )
}

#[derive(Clone, Copy)]
enum ButtonVariant {
    Primary,
    Secondary,
}

fn control_button(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    key: &SmolStr,
    suffix: &str,
    theme: &ThemeHandle,
    label: SmolStr,
    control: DialogControl,
    variant: ButtonVariant,
    disabled: bool,
    updater: &Weak<RefCell<UpdaterCore>>,
) -> Result<Button, EngineError> {
    let button = Button::new(app, theme.clone(), format_smolstr!("{key}.{suffix}"))
        .a11y_label(label.clone())
        .label(label)
        .disabled(disabled);
    let button = match variant {
        ButtonVariant::Primary => button.primary(),
        ButtonVariant::Secondary => button.secondary(),
    };
    let button = if disabled {
        button
    } else {
        button.on_click(control_callback(updater, control))
    };
    button.build(app, window, parent)?;
    Ok(button)
}

fn release_page_button(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    key: &SmolStr,
    theme: &ThemeHandle,
    translate: &Translate,
    updater: &Weak<RefCell<UpdaterCore>>,
) -> Result<Button, EngineError> {
    let label = t_smol!(translate, "window.updater.open_release_page");
    let opener = updater.clone();
    let button = Button::new(app, theme.clone(), format_smolstr!("{key}.release-page"))
        .primary()
        .a11y_label(label.clone())
        .label(label)
        .on_click(move |_ctx| {
            let Some(core) = opener.upgrade() else {
                return;
            };
            let open_url = core.borrow().services().open_url.clone();
            open_url(publish::RELEASE_PAGE);
        });
    button.build(app, window, parent)?;
    Ok(button)
}

struct ReleaseNotesAssets;

impl MarkdownAssets for ReleaseNotesAssets {
    fn load(&self, _path: &str) -> Option<std::borrow::Cow<'static, [u8]>> {
        None
    }
}

fn build_node(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    build: impl FnOnce(&mut NodeTree, NodeId) -> Result<NodeId, EngineError>,
) -> Result<NodeId, EngineError> {
    build(tree_mut(app, window)?, parent)
}

fn tree_mut(app: &mut App, window: WindowId) -> Result<&mut NodeTree, EngineError> {
    Ok(app
        .window_mut(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .tree_mut())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use download_provider::task::DownloadId;

    use super::*;
    use crate::{
        error::UpdaterError,
        state::UpdaterState,
        test_support::{Harness, release, release_with_notes},
        updater::{DownloadPhase, Updater, UpdaterTestExt, UpdaterView},
    };

    fn shown(view: UpdaterView) -> (Harness, Updater) {
        let mut harness = Harness::new();
        let updater = harness.install_updater(None);
        updater.test_cancel_check(harness.app.app_mut());
        set_view(&mut harness, &updater, view);
        updater.open_dialog(harness.app.app_mut());
        harness.settle(4);
        (harness, updater)
    }

    fn set_view(harness: &mut Harness, updater: &Updater, view: UpdaterView) {
        let state = match view {
            UpdaterView::Notes => UpdaterState::Available(release("1.2.3")),
            UpdaterView::Download {
                downloaded,
                total,
                phase,
            } => {
                let task = DownloadId::new();
                match phase {
                    DownloadPhase::Downloading => UpdaterState::downloading_for_test(
                        release("1.2.3"),
                        task,
                        downloaded,
                        total,
                    ),
                    DownloadPhase::Pausing => {
                        UpdaterState::pausing_for_test(release("1.2.3"), task, downloaded, total)
                    }
                    DownloadPhase::Paused => {
                        UpdaterState::paused_for_test(release("1.2.3"), task, downloaded, total)
                    }
                }
            }
            UpdaterView::Downloaded => UpdaterState::ready_for_test(
                release("1.2.3"),
                bytes::Bytes::from_static(b"payload"),
            ),
            UpdaterView::Installing => UpdaterState::Installing(release("1.2.3")),
            UpdaterView::Failure(_) => UpdaterState::failed_for_test(
                release("1.2.3"),
                crate::error::UpdaterError::UnsupportedPlatform,
            ),
        };
        updater.test_set_state(harness.app.app_mut(), state);
    }

    fn content(harness: &Harness) -> NodeId {
        harness
            .find("test.updater.dialog:content")
            .unwrap_or_else(|| {
                panic!(
                    "the dialog draws its content (overlays: {}, text: {:?})",
                    harness.popup_count(),
                    harness.text_of(harness.root())
                )
            })
    }

    fn press(harness: &mut Harness, updater: &Updater, suffix: &str) {
        assert!(
            updater.test_press(harness.app.app_mut(), suffix),
            "the dialog offers a {suffix} control"
        );
        harness.settle(2);
    }

    fn settle_close(harness: &mut Harness, updater: &Updater) {
        for _ in 0..16 {
            if !updater.test_dialog_open(harness.app.app()) {
                return;
            }
            harness.advance(Duration::from_millis(40));
        }
        panic!("the dialog should have closed");
    }

    #[test]
    fn a_dialog_should_draw_the_notes_and_offer_the_download() {
        let (harness, _updater) = shown(UpdaterView::Notes);
        let content = content(&harness);

        assert!(
            harness.text_of(content).contains("What changed"),
            "the notes are what the dialog is opened for: {:?}",
            harness.text_of(content)
        );
        assert!(
            harness.find("test.updater.dialog.download").is_some(),
            "an installable payload is offered as a download"
        );
        assert!(
            harness.find("test.updater.dialog.cancel").is_some(),
            "closing the offer is always possible"
        );
    }

    #[test]
    fn the_dialog_should_draw_the_notes_in_the_language_the_interface_is_in() {
        let mut harness = Harness::new();
        let updater = harness.install_updater(None);
        updater.test_cancel_check(harness.app.app_mut());
        harness.switch_language(i18n::language::codes::LanguageCode::ZhCn);
        updater.test_set_state(
            harness.app.app_mut(),
            UpdaterState::Available(release_with_notes(&[
                ("en", "## What changed\n\n- everything"),
                ("zh-CN", "## 更新内容\n\n- 全部"),
            ])),
        );
        updater.open_dialog(harness.app.app_mut());
        harness.settle(4);

        let text = harness.text_of(content(&harness));
        assert!(
            text.contains("更新内容"),
            "the notes follow the interface's language: {text:?}"
        );
    }

    #[test]
    fn the_dialog_should_fall_back_to_the_english_notes() {
        let (harness, updater) = {
            let mut harness = Harness::new();
            let updater = harness.install_updater(None);
            updater.test_cancel_check(harness.app.app_mut());
            harness.switch_language(i18n::language::codes::LanguageCode::ZhCn);
            updater.test_set_state(
                harness.app.app_mut(),
                UpdaterState::Available(release_with_notes(&[(
                    "en",
                    "## English only\n\n- one line",
                )])),
            );
            updater.open_dialog(harness.app.app_mut());
            harness.settle(4);
            (harness, updater)
        };

        let text = harness.text_of(content(&harness));
        assert!(
            text.contains("English only"),
            "the fallback notes are drawn: {text:?}"
        );
        let _ = updater;
    }

    #[test]
    fn a_download_in_flight_should_be_drawn_as_progress_and_offer_pausing_and_cancelling() {
        let (mut harness, updater) = shown(UpdaterView::Download {
            downloaded: 4_096,
            total: Some(8_192),
            phase: DownloadPhase::Downloading,
        });
        let content = content(&harness);
        let text = harness.text_of(content);

        assert!(
            text.contains("Downloading the update"),
            "the state says what is happening: {text:?}"
        );
        assert!(
            text.contains("50%"),
            "what the transfer accepted is drawn against the size it named: {text:?}"
        );
        assert!(
            harness.find("test.updater.dialog:progress").is_some(),
            "a named length is drawn as a bar"
        );
        assert!(
            harness.find("test.updater.dialog.pause").is_some(),
            "a running download can be paused"
        );
        assert!(
            harness.find("test.updater.dialog.cancel").is_some(),
            "a running download can be given up"
        );
        assert!(
            harness.find("test.updater.dialog.install").is_none(),
            "nothing is downloaded yet"
        );

        press(&mut harness, &updater, "cancel");
        assert_eq!(
            updater.view(harness.app.app()),
            Some(UpdaterView::Notes),
            "cancelling a download offers the release again"
        );
        settle_close(&mut harness, &updater);
    }

    #[test]
    fn a_download_being_paused_should_say_so_and_offer_cancelling() {
        let (harness, _updater) = shown(UpdaterView::Download {
            downloaded: 4_096,
            total: Some(8_192),
            phase: DownloadPhase::Pausing,
        });
        let text = harness.text_of(content(&harness));

        assert!(text.contains("Pausing"), "the state says so: {text:?}");
        assert!(
            harness.find("test.updater.dialog.pausing").is_some(),
            "the control the dialog is drawn with says the transfer is being stopped"
        );
        assert!(
            harness.find("test.updater.dialog.pause").is_none(),
            "there is nothing left to pause"
        );
        assert!(
            harness.find("test.updater.dialog.resume").is_none(),
            "the transfer has not stopped yet"
        );
        assert!(
            harness.find("test.updater.dialog.cancel").is_some(),
            "a download being stopped can still be given up"
        );
    }

    #[test]
    fn a_paused_download_should_offer_resuming_and_cancelling() {
        let (harness, _updater) = shown(UpdaterView::Download {
            downloaded: 4_096,
            total: Some(8_192),
            phase: DownloadPhase::Paused,
        });
        let text = harness.text_of(content(&harness));

        assert!(text.contains("Paused"), "the state says so: {text:?}");
        assert!(
            harness.find("test.updater.dialog.pause").is_none(),
            "a stopped download has nothing left to pause"
        );
        assert!(
            harness.find("test.updater.dialog.resume").is_some(),
            "a stopped download can be continued"
        );
        assert!(
            harness.find("test.updater.dialog.cancel").is_some(),
            "a stopped download can be given up"
        );
    }

    #[test]
    fn a_download_without_a_length_should_be_drawn_as_something_happening() {
        let (harness, _updater) = shown(UpdaterView::Download {
            downloaded: 4_096,
            total: None,
            phase: DownloadPhase::Downloading,
        });
        let content = content(&harness);

        assert!(
            harness.find("test.updater.dialog:spinner-row").is_some(),
            "a chunked response is drawn as something happening"
        );
        assert!(
            harness.find("test.updater.dialog:progress").is_none(),
            "a length that is not known is not drawn as a bar"
        );
        let _ = content;
    }

    #[test]
    fn a_downloaded_payload_should_offer_the_install_and_be_given_up_on_closing() {
        let (mut harness, updater) = shown(UpdaterView::Downloaded);
        let text = harness.text_of(content(&harness));

        assert!(
            text.contains("The new version has been downloaded"),
            "a downloaded payload says so: {text:?}"
        );
        assert!(
            harness.find("test.updater.dialog.install").is_some(),
            "what a downloaded payload is for is installing it"
        );

        press(&mut harness, &updater, "cancel");
        assert_eq!(
            updater.view(harness.app.app()),
            Some(UpdaterView::Notes),
            "giving the payload up offers the release again"
        );
        settle_close(&mut harness, &updater);
    }

    #[test]
    fn a_failed_attempt_should_be_drawn_where_the_notes_were() {
        let (mut harness, updater) = shown(UpdaterView::Failure(
            UpdaterError::UnsupportedPlatform.message_key(),
        ));
        let text = harness.text_of(content(&harness));

        assert!(
            text.contains("This platform does not support in-app updates"),
            "the failure is stated in the language the interface is in: {text:?}"
        );
        assert!(
            harness.find("test.updater.dialog.download").is_none(),
            "a failure is not an invitation to start over"
        );
        assert!(
            harness.find("test.updater.dialog.dismiss").is_some(),
            "the dialog can be dismissed"
        );

        press(&mut harness, &updater, "dismiss");
        assert_eq!(
            updater.view(harness.app.app()),
            Some(UpdaterView::Notes),
            "closing on a failure leaves the release on offer"
        );
        settle_close(&mut harness, &updater);
    }

    #[test]
    fn the_notes_should_be_drawn_wider_than_the_status_the_dialog_turns_into() {
        let (harness, _updater) = shown(UpdaterView::Notes);
        let notes = harness
            .app
            .probe()
            .bounds(content(&harness))
            .expect("the notes panel is laid out");
        drop(harness);

        let (harness, _updater) = shown(UpdaterView::Download {
            downloaded: 4_096,
            total: Some(8_192),
            phase: DownloadPhase::Downloading,
        });
        let status = harness
            .app
            .probe()
            .bounds(content(&harness))
            .expect("the status panel is laid out");

        assert!(
            notes.size.width.value() > 900.0,
            "the notes fill the wide panel ({:?})",
            notes.size
        );
        assert!(
            (status.size.width - STATUS_WIDTH).abs().value() < 0.5,
            "the status content fills the narrower panel ({:?})",
            status.size
        );
        assert!(
            notes.size.width > status.size.width,
            "the notes are drawn wider than the status"
        );
    }

    #[test]
    fn the_notes_should_be_drawn_with_their_scrollbar() {
        let mut harness = Harness::new();
        let updater = harness.install_updater(None);
        updater.test_cancel_check(harness.app.app_mut());
        let long_notes =
            "## What changed\n\n".to_string() + &"A paragraph of the release notes. ".repeat(200);
        let long_notes: &'static str = Box::leak(long_notes.into_boxed_str());
        updater.test_set_state(
            harness.app.app_mut(),
            UpdaterState::Available(release_with_notes(&[("en", long_notes)])),
        );
        updater.open_dialog(harness.app.app_mut());
        harness.settle(4);

        let scrollbar = harness
            .find("test.updater.dialog:notes-scrollbar")
            .expect("the scrollbar is drawn beside the notes");
        let bounds = harness
            .app
            .probe()
            .bounds(scrollbar)
            .expect("the scrollbar is laid out");
        assert!(
            bounds.size.height.value() > 100.0,
            "the scrollbar runs the height of the notes it scrolls ({bounds:?})"
        );
    }

    #[test]
    fn a_dismissed_dialog_should_keep_drawing_the_frame_it_was_dismissed_in() {
        let (mut harness, updater) = shown(UpdaterView::Download {
            downloaded: 4_096,
            total: Some(8_192),
            phase: DownloadPhase::Downloading,
        });
        harness.advance(Duration::from_millis(300));
        let content_before = content(&harness);
        assert!(harness.text_of(content_before).contains("Downloading"));

        updater.test_press(harness.app.app_mut(), "cancel");
        harness.drain();

        assert_eq!(
            updater.view(harness.app.app()),
            Some(UpdaterView::Notes),
            "cancelling offers the release again straight away"
        );
        assert!(
            updater.test_dialog_closing(harness.app.app()),
            "the dialog keeps drawing while its exit plays"
        );
        let content_after = content(&harness);
        assert!(
            harness.text_of(content_after).contains("Downloading"),
            "the dialog that is still fading is the download it was dismissed in, not the notes"
        );
        assert!(
            !harness.text_of(content_after).contains("What changed"),
            "the notes the release went back to are not what the fade draws"
        );
        settle_close(&mut harness, &updater);
    }

    #[test]
    fn a_language_switch_should_rebuild_the_dialog_copy() {
        let (mut harness, updater) = shown(UpdaterView::Download {
            downloaded: 4_096,
            total: Some(8_192),
            phase: DownloadPhase::Downloading,
        });
        assert!(
            harness
                .text_of(content(&harness))
                .contains("Downloading the update")
        );

        harness.switch_language(i18n::language::codes::LanguageCode::ZhCn);
        harness.settle(3);

        let text = harness.text_of(content(&harness));
        assert!(
            text.contains("正在下载更新"),
            "the dialog's copy follows the language: {text:?}"
        );
        assert!(
            updater.test_dialog_open(harness.app.app()),
            "rebuilding the copy keeps the dialog open"
        );
    }

    #[test]
    fn releasing_the_updater_should_reclaim_the_dialog_it_built() {
        let mut harness = Harness::new();
        let baseline = harness.signal_slot_stats();
        let routes = harness.router.len();
        let updater = harness.install_updater(None);
        updater.test_cancel_check(harness.app.app_mut());
        updater.test_set_state(
            harness.app.app_mut(),
            UpdaterState::Available(release("1.2.3")),
        );
        updater.open_dialog(harness.app.app_mut());
        harness.settle(4);
        assert!(updater.test_dialog_open(harness.app.app()));
        assert!(
            harness.popup_count() > 0,
            "the dialog is in the window's popup stack"
        );

        updater.release(harness.app.app_mut());

        assert!(
            harness.popup_count() == 0,
            "releasing removes the dialog from the same stack"
        );
        assert_eq!(
            harness.popup_total(),
            0,
            "releasing removes the dialog's scrim together with it"
        );
        assert_eq!(harness.router.len(), routes);
        assert_eq!(
            harness.signal_slot_stats().live,
            baseline.live,
            "releasing reclaims every value the dialog minted"
        );
    }
}
