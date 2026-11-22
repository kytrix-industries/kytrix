//! The supported translation models, drawn as a table: how large each model's file is, and what
//! the row can do with it.
//!
//! The table is native UI built from the page's own rows rather than `kit::table`: a table cell is
//! built by a closure that is given the node tree but no `&mut App`, and a row's actions are kit
//! buttons — components that register their action handlers on `App`. Eight fixed rows are also
//! small enough to build and rebuild directly, so the row content follows the download state by
//! rebuilding the status cell of the row that changed.
//!
//! A row does not download, resume, stop or delete anything itself: its buttons call the injected
//! [`TranslationDownloads`], and what the manager last reported is what the row draws.

use std::rc::Rc;

use app_config::translation::{DownloadSite, TranslationModel};
use base_ui::{
    kit::{
        button::Button,
        icon::IconKind,
        size::{UnifiedSizable, UnifiedSize},
    },
    style_builder::{PaintBuilder, StyleBuilder},
    theme::{appearance::Appearance, handle::ThemeHandle, role::Role},
};
use download_provider::error::DownloadFailure;
use i18n::{t_smol, translate::Translate};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    node::{builder::NodeBuilder, id::NodeId},
    paint::style::{PaintSpec, StylePatch},
    text::{TextOverflow, TextStyle, TextWrap},
    window::WindowId,
};

use crate::{
    layout,
    translation_downloads::{DeleteFailure, DownloadState, TranslationDownloads, UnloadFailure},
};

/// The height of the header row.
const HEADER_HEIGHT: f32 = 28.0;
/// The height of one model row.
const ROW_HEIGHT: f32 = 34.0;
/// The width of the size column.
const SIZE_COLUMN_WIDTH: f32 = 110.0;
/// The width of the status column: its two action buttons and the line of text beside them.
const STATUS_COLUMN_WIDTH: f32 = 340.0;
/// The size of a row's text.
const TEXT_SIZE: f32 = 11.5;
/// The appearance a bound colour is seeded from before its style pass evaluates the patch.
const SEED_APPEARANCE: Appearance = Appearance::Dark;

/// The model, whether its file is in place, and what the download manager last reported for it.
#[derive(Clone, Debug, PartialEq)]
struct ModelRow {
    model: TranslationModel,
    downloaded: bool,
    state: DownloadState,
    /// The site a download of this row starts from.
    site: DownloadSite,
    /// Whether a download can be started: both the manager and a directory have to exist.
    can_download: bool,
}

/// The models, drawn as a table.
pub(crate) struct TranslationTable {
    /// The key prefix every kit component of the table names itself by.
    key: SmolStr,
    /// The process-wide downloads, when the composition host attached them.
    downloads: Option<Rc<TranslationDownloads>>,
    rows: Vec<RowView>,
    /// The rows the table was last drawn for, so a frame that moved nothing rebuilds nothing.
    applied: Vec<ModelRow>,
}

/// One model's row as it is drawn.
struct RowView {
    status: NodeId,
    buttons: Vec<Button>,
}

impl TranslationTable {
    /// The table over the process-wide downloads, keyed by the settings page instance it belongs to.
    pub(crate) fn new(key: SmolStr, downloads: Option<Rc<TranslationDownloads>>) -> Self {
        Self {
            key,
            downloads,
            rows: Vec::new(),
            applied: Vec::new(),
        }
    }

    /// Builds the table under `parent`.
    ///
    /// # Errors
    /// Engine node-building errors, or a kit button that refused to build.
    pub(crate) fn build(
        &mut self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        theme: &ThemeHandle,
        translate: &Translate,
        directory: Option<&std::path::Path>,
        site: DownloadSite,
    ) -> Result<(), EngineError> {
        let table = layout::themed_box(
            app,
            window,
            parent,
            SmolStr::new_static("SETTINGS_AI_MODEL_TABLE"),
            StyleBuilder::new()
                .flex_col()
                .flex_1()
                .width_full()
                .min_width_0()
                .min_height_0()
                .overflow_hidden()
                .build(),
            PaintBuilder::new().rounded(8.0).build(),
            theme,
            Some(Role::SurfaceSunken),
            Some(Role::LineDivider),
        )?;
        self.build_header(app, window, table, theme, translate)?;
        let rows = layout::build_node(app, window, table, |tree, parent| {
            NodeBuilder::boxed()
                .id(SmolStr::new_static("SETTINGS_AI_MODEL_ROWS"))
                .style(
                    StyleBuilder::new()
                        .flex_col()
                        .flex_none()
                        .width_full()
                        .min_width_0()
                        .build(),
                )
                .build(tree, parent)
        })?;
        self.rows.clear();
        self.applied.clear();
        for (index, row) in model_rows(&self.downloads, directory, site)
            .into_iter()
            .enumerate()
        {
            self.rows
                .push(self.build_row(app, window, rows, theme, translate, index, &row, directory)?);
            self.applied.push(row);
        }

        Ok(())
    }

    /// Draws one row's content again when what it shows changed.
    ///
    /// # Errors
    /// Engine node-building errors, or a kit button that refused to build.
    pub(crate) fn sync(
        &mut self,
        app: &mut App,
        window: WindowId,
        theme: &ThemeHandle,
        translate: &Translate,
        directory: Option<&std::path::Path>,
        site: DownloadSite,
    ) -> Result<bool, EngineError> {
        let wanted = model_rows(&self.downloads, directory, site);
        if wanted == self.applied {
            return Ok(false);
        }

        for (index, (view, row)) in self.rows.iter_mut().zip(&wanted).enumerate() {
            if self.applied.get(index) == Some(row) {
                continue;
            }
            for button in view.buttons.drain(..) {
                button.release(app);
            }
            remove_children(app, window, view.status);
            build_status(
                app,
                window,
                view.status,
                theme,
                translate,
                &self.key,
                index,
                row,
                directory,
                &self.downloads,
                &mut view.buttons,
            )?;
        }
        self.applied = wanted;

        Ok(true)
    }

    /// Releases every kit component the table owns.
    pub(crate) fn release(&mut self, app: &mut App) {
        for row in &mut self.rows {
            for button in row.buttons.drain(..) {
                button.release(app);
            }
        }
        self.rows.clear();
        self.applied.clear();
    }

    /// Builds the header row: the three column names the rows line up under.
    fn build_header(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        theme: &ThemeHandle,
        translate: &Translate,
    ) -> Result<(), EngineError> {
        let header = layout::build_node(app, window, parent, |tree, parent| {
            NodeBuilder::boxed()
                .id(SmolStr::new_static("SETTINGS_AI_MODEL_HEADER"))
                .style(
                    StyleBuilder::new()
                        .flex_row()
                        .items_center()
                        .flex_none()
                        .width_full()
                        .height(HEADER_HEIGHT)
                        .padding_x(12.0)
                        .gap(8.0)
                        .build(),
                )
                .build(tree, parent)
        })?;
        for (id, key, width) in [
            ("SETTINGS_AI_COLUMN_NAME", "settings_ui.ai.model_name", None),
            (
                "SETTINGS_AI_COLUMN_SIZE",
                "settings_ui.ai.size",
                Some(SIZE_COLUMN_WIDTH),
            ),
            (
                "SETTINGS_AI_COLUMN_STATUS",
                "settings_ui.ai.status",
                Some(STATUS_COLUMN_WIDTH),
            ),
        ] {
            let style = match width {
                Some(width) => StyleBuilder::new()
                    .flex_none()
                    .width(width)
                    .min_width_0()
                    .build(),
                None => StyleBuilder::new().flex_1().min_width_0().build(),
            };
            let cell = layout::container(app, window, header, style)?;
            layout::themed_text(
                app,
                window,
                cell,
                Some(SmolStr::new_static(id)),
                t_smol!(translate, key),
                TEXT_SIZE,
                500,
                Role::FgMuted,
                theme,
            )?;
        }

        Ok(())
    }

    /// Builds one model's row: its name and size, and the cell its actions are drawn in.
    fn build_row(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        theme: &ThemeHandle,
        translate: &Translate,
        index: usize,
        row: &ModelRow,
        directory: Option<&std::path::Path>,
    ) -> Result<RowView, EngineError> {
        let container = layout::themed_box(
            app,
            window,
            parent,
            format_smolstr!("SETTINGS_AI_MODEL_ROW_{index}"),
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .flex_none()
                .width_full()
                .height(ROW_HEIGHT)
                .min_width_0()
                .padding_x(12.0)
                .gap(8.0)
                .build(),
            PaintSpec::default(),
            theme,
            None,
            None,
        )?;
        let name = layout::container(
            app,
            window,
            container,
            StyleBuilder::new().flex_1().min_width_0().build(),
        )?;
        single_line_text(
            app,
            window,
            name,
            format_smolstr!("SETTINGS_AI_MODEL_{index}"),
            SmolStr::new(row.model.display_name()),
            Role::FgDefault,
            500,
            theme,
        )?;
        let size = layout::container(
            app,
            window,
            container,
            StyleBuilder::new()
                .flex_none()
                .width(SIZE_COLUMN_WIDTH)
                .min_width_0()
                .build(),
        )?;
        single_line_text(
            app,
            window,
            size,
            format_smolstr!("SETTINGS_AI_MODEL_SIZE_{index}"),
            size_text(row.model.size_bytes()),
            Role::FgMuted,
            400,
            theme,
        )?;
        let status = layout::build_node(app, window, container, |tree, parent| {
            NodeBuilder::boxed()
                .id(format_smolstr!("SETTINGS_AI_MODEL_STATUS_{index}"))
                .style(
                    StyleBuilder::new()
                        .flex_row()
                        .items_center()
                        .flex_none()
                        .width(STATUS_COLUMN_WIDTH)
                        .min_width_0()
                        .gap(4.0)
                        .build(),
                )
                .build(tree, parent)
        })?;
        let mut buttons = Vec::new();
        build_status(
            app,
            window,
            status,
            theme,
            translate,
            &self.key,
            index,
            row,
            directory,
            &self.downloads,
            &mut buttons,
        )?;

        Ok(RowView { status, buttons })
    }
}

/// The model rows as the settings and the download manager describe them now.
fn model_rows(
    downloads: &Option<Rc<TranslationDownloads>>,
    directory: Option<&std::path::Path>,
    site: DownloadSite,
) -> Vec<ModelRow> {
    TranslationModel::ALL
        .into_iter()
        .map(|model| {
            let (downloaded, state) = match (downloads.as_deref(), directory) {
                (Some(downloads), Some(directory)) => {
                    let state = downloads.row_state(model, directory);
                    // A transfer in progress says whether the file is there: its state is what
                    // the row draws, so only an idle row pays for looking at the disk.
                    let downloaded = matches!(state, DownloadState::Idle)
                        && downloads.is_downloaded(model, directory);
                    (downloaded, state)
                }
                _ => (false, DownloadState::Idle),
            };
            ModelRow {
                model,
                downloaded,
                state,
                site,
                can_download: downloads.is_some() && directory.is_some(),
            }
        })
        .collect()
}

/// Draws the actions and the text of one row's status cell.
fn build_status(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    theme: &ThemeHandle,
    translate: &Translate,
    key: &str,
    index: usize,
    row: &ModelRow,
    directory: Option<&std::path::Path>,
    downloads: &Option<Rc<TranslationDownloads>>,
    buttons: &mut Vec<Button>,
) -> Result<(), EngineError> {
    let model = row.model;
    let mut text = None;
    let mut action = |action: RowAction, disabled: bool| -> Result<(), EngineError> {
        let button = action_button(
            app, window, parent, theme, key, index, action, disabled, translate, model, row.site,
            directory, downloads,
        )?;
        buttons.push(button);
        Ok(())
    };

    match &row.state {
        DownloadState::Deleting => action(RowAction::Deleting, true)?,
        DownloadState::Terminating => {
            action(RowAction::Terminating, true)?;
            text = Some((
                t_smol!(translate, "settings_ui.ai.terminating"),
                Role::FgMuted,
            ));
        }
        DownloadState::Downloading {
            downloaded,
            mb_per_second,
        } => {
            action(RowAction::Pause, false)?;
            action(RowAction::Terminate, false)?;
            text = Some((
                t_smol!(
                    translate,
                    "settings_ui.ai.downloading_progress",
                    speed = speed_text(*mb_per_second),
                    percent = whole_percent(model, *downloaded)
                ),
                Role::FgMuted,
            ));
        }
        DownloadState::Pausing { .. } => {
            action(RowAction::Pause, true)?;
            action(RowAction::Terminate, false)?;
            text = Some((t_smol!(translate, "settings_ui.ai.pausing"), Role::FgMuted));
        }
        DownloadState::Paused { downloaded } => {
            // A paused download is either continued from its partial file or stopped for good:
            // there is nothing else the row can do with bytes that are neither in place nor
            // wanted.
            action(RowAction::Resume, false)?;
            action(RowAction::Terminate, false)?;
            text = Some((
                t_smol!(
                    translate,
                    "settings_ui.ai.paused_progress",
                    percent = whole_percent(model, *downloaded)
                ),
                Role::FgMuted,
            ));
        }
        DownloadState::DownloadFailed { failure } => {
            action(RowAction::Retry, false)?;
            action(RowAction::Terminate, false)?;
            text = Some((
                t_smol!(
                    translate,
                    "settings_ui.ai.download_failed",
                    reason = download_failure_reason(translate, failure)
                ),
                Role::StatusDangerBase,
            ));
        }
        DownloadState::DeleteFailed { failure } => {
            action(RowAction::Delete, false)?;
            text = Some((
                t_smol!(
                    translate,
                    "settings_ui.ai.delete_failed",
                    reason = delete_failure_reason(translate, failure)
                ),
                Role::StatusDangerBase,
            ));
        }
        DownloadState::TerminateFailed { failure } => {
            action(RowAction::Terminate, false)?;
            text = Some((
                t_smol!(
                    translate,
                    "settings_ui.ai.terminate_failed",
                    reason = delete_failure_reason(translate, failure)
                ),
                Role::StatusDangerBase,
            ));
        }
        DownloadState::Idle => {
            if row.downloaded {
                action(RowAction::Delete, false)?;
            } else {
                action(RowAction::Download, !row.can_download)?;
            }
        }
    }

    if let Some((caption, role)) = text {
        let id = match role {
            Role::StatusDangerBase => format_smolstr!("SETTINGS_AI_MODEL_ERROR_{index}"),
            _ => format_smolstr!("SETTINGS_AI_MODEL_PROGRESS_{index}"),
        };
        single_line_text(app, window, parent, id, caption, role, 400, theme)?;
    }

    Ok(())
}

/// One icon-only action button of a row.
fn action_button(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    theme: &ThemeHandle,
    key: &str,
    index: usize,
    action: RowAction,
    disabled: bool,
    translate: &Translate,
    model: TranslationModel,
    site: DownloadSite,
    directory: Option<&std::path::Path>,
    downloads: &Option<Rc<TranslationDownloads>>,
) -> Result<Button, EngineError> {
    let label = t_smol!(translate, action.label_key());
    let button_key = format_smolstr!("{key}.{index}.{}", action.key_suffix());
    let mut button = Button::new(app, theme.clone(), button_key)
        .ghost()
        .with_unified_size(UnifiedSize::Small)
        .icon(action.icon())
        .a11y_label(label.clone())
        .tooltip_text(label)
        .disabled(disabled);

    match (downloads.as_ref(), directory) {
        (Some(downloads), Some(directory)) if !disabled => {
            let downloads = Rc::clone(downloads);
            let directory = directory.to_path_buf();
            button = button.on_click(move |ctx| {
                let app = ctx.app_mut();
                match action {
                    RowAction::Download => downloads.download(model, directory.clone(), site),
                    RowAction::Pause => {
                        downloads.pause(model, directory.clone());
                    }
                    RowAction::Resume => {
                        downloads.resume(model, directory.clone());
                    }
                    RowAction::Retry => downloads.retry(app, model, directory.clone(), site),
                    RowAction::Terminate => downloads.terminate(app, model, directory.clone()),
                    RowAction::Delete => downloads.delete(app, model, directory.clone()),
                    RowAction::Deleting | RowAction::Terminating => {}
                }
            });
        }
        _ => button = button.disabled(true),
    }

    let wrapper = layout::build_node(app, window, parent, |tree, parent| {
        NodeBuilder::boxed()
            .id(format_smolstr!(
                "SETTINGS_AI_MODEL_{}_{index}",
                action.debug_suffix()
            ))
            .style(StyleBuilder::new().flex_none().build())
            .build(tree, parent)
    })?;
    button.build(app, window, wrapper)?;

    Ok(button)
}

/// One line of text that is cut rather than wrapped, in the theme's foreground role.
fn single_line_text(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    id: SmolStr,
    text: SmolStr,
    role: Role,
    weight: u16,
    theme: &ThemeHandle,
) -> Result<(), EngineError> {
    let container = layout::container(
        app,
        window,
        parent,
        StyleBuilder::new().flex_1().min_width_0().build(),
    )?;
    let style = TextStyle::new(TEXT_SIZE)
        .with_weight(weight)
        .with_color(theme.theme_for(SEED_APPEARANCE).role(role))
        .with_wrap(TextWrap::NoWrap)
        .with_overflow(TextOverflow::Ellipsis, 1);
    let theme = theme.clone();
    layout::build_node(app, window, container, move |tree, parent| {
        NodeBuilder::text(text, style)
            .id(id)
            .style(StyleBuilder::new().width_full().build())
            .bind_style(move |cx| StylePatch {
                foreground: Some(theme.role(cx, role)),
                ..StylePatch::default()
            })
            .build(tree, parent)
    })?;

    Ok(())
}

/// Removes every child of `parent`, which is how a rebuilt status cell is cleaned.
fn remove_children(app: &mut App, window: WindowId, parent: NodeId) {
    let children = app
        .window(window)
        .and_then(|target| target.tree().get(parent).ok())
        .map(|entry| entry.children().to_vec())
        .unwrap_or_default();
    for child in children {
        if let Err(error) = app.remove_node(window, child) {
            log::warn!("removing a rebuilt translation row failed: {error}");
        }
    }
}

/// The icon-only action one row draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowAction {
    Download,
    Pause,
    Resume,
    Retry,
    /// Stops a download for good and discards what it wrote.
    Terminate,
    Delete,
    /// The disabled bin a row is drawn with while its delete is in flight.
    Deleting,
    /// The disabled stop a row is drawn with while its terminate is in flight.
    Terminating,
}

impl RowAction {
    /// The i18n key the action's label and tooltip are drawn with.
    pub(crate) fn label_key(self) -> &'static str {
        match self {
            Self::Download => "settings_ui.ai.download",
            Self::Pause => "settings_ui.ai.pause",
            Self::Resume => "settings_ui.ai.resume",
            Self::Retry => "settings_ui.ai.retry",
            Self::Terminate | Self::Terminating => "settings_ui.ai.terminate",
            Self::Delete => "settings_ui.ai.delete",
            Self::Deleting => "settings_ui.ai.deleting",
        }
    }

    /// The picture the action is drawn with.
    pub(crate) fn icon(self) -> IconKind {
        match self {
            Self::Download => IconKind::DownloadSimple,
            Self::Pause => IconKind::Pause,
            Self::Resume => IconKind::Play,
            Self::Retry => IconKind::ArrowsClockwise,
            Self::Terminate | Self::Terminating => IconKind::StopFill,
            Self::Delete | Self::Deleting => IconKind::Trash,
        }
    }

    /// The suffix one action's kit key is named by.
    fn key_suffix(self) -> &'static str {
        match self {
            Self::Download => "download",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Retry => "retry",
            Self::Terminate => "terminate",
            Self::Delete => "delete",
            Self::Deleting => "deleting",
            Self::Terminating => "terminating",
        }
    }

    /// The suffix of the debug id one action's button is wrapped in.
    fn debug_suffix(self) -> &'static str {
        match self {
            Self::Download => "DOWNLOAD",
            Self::Pause => "PAUSE",
            Self::Resume => "RESUME",
            Self::Retry => "RETRY",
            Self::Terminate => "TERMINATE",
            Self::Delete => "DELETE",
            Self::Deleting => "DELETING",
            Self::Terminating => "TERMINATING",
        }
    }
}

/// The published size of a model's file, in the unit that keeps the number short: megabytes up
/// to 1024 of them, and gigabytes above that. Two decimals are always written, rounded.
pub(crate) fn size_text(bytes: u64) -> SmolStr {
    const MEBIBYTE: u64 = 1024 * 1024;
    const GIBIBYTE: u64 = 1024 * MEBIBYTE;

    let (value, unit) = if bytes > GIBIBYTE {
        (bytes as f64 / GIBIBYTE as f64, "GB")
    } else {
        (bytes as f64 / MEBIBYTE as f64, "MB")
    };

    format_smolstr!("{value:.2} {unit}")
}

/// The rate a download is transferring at, in the unit that keeps the number short: kilobytes
/// below one megabyte per second, megabytes up to 1024 of them, and gigabytes above that. At
/// most two decimals are written, and trailing zeros are dropped.
pub(crate) fn speed_text(megabytes_per_second: f32) -> SmolStr {
    let (value, unit) = if megabytes_per_second < 1.0 {
        (megabytes_per_second * 1024.0, "KB/s")
    } else if megabytes_per_second > 1024.0 {
        (megabytes_per_second / 1024.0, "GB/s")
    } else {
        (megabytes_per_second, "MB/s")
    };
    let rounded = format!("{:.2}", value.max(0.0));
    let number = rounded.trim_end_matches('0').trim_end_matches('.');

    format_smolstr!("{number} {unit}")
}

/// How much of `model`'s published file `downloaded` bytes are, as a whole percentage.
pub(crate) fn whole_percent(model: TranslationModel, downloaded: u64) -> u32 {
    let total = model.size_bytes();
    if total == 0 {
        return 0;
    }

    ((downloaded as f64 / total as f64) * 100.0)
        .clamp(0.0, 100.0)
        .round() as u32
}

/// The text a download failure is drawn with, in the interface's language.
fn download_failure_reason(translate: &Translate, failure: &DownloadFailure) -> SmolStr {
    match failure {
        DownloadFailure::Status { status } => {
            t_smol!(
                translate,
                "settings_ui.ai.download_failed_status",
                status = status
            )
        }
        // The transport that could not be started, the request that could not be sent and the
        // body that could not be read are one message to the reader: the download failed on the
        // network.
        DownloadFailure::Transport | DownloadFailure::Request | DownloadFailure::Read => {
            t_smol!(translate, "settings_ui.ai.download_failed_transport")
        }
        DownloadFailure::Route => t_smol!(translate, "settings_ui.ai.download_failed_route"),
        DownloadFailure::Range => t_smol!(translate, "settings_ui.ai.download_failed_range"),
        DownloadFailure::SizeMismatch { expected, actual } => t_smol!(
            translate,
            "settings_ui.ai.download_failed_size",
            expected = expected,
            actual = actual
        ),
        DownloadFailure::TooLarge { .. } => {
            t_smol!(translate, "settings_ui.ai.download_failed_too_large")
        }
        DownloadFailure::Cancelled => {
            t_smol!(translate, "settings_ui.ai.download_failed_cancelled")
        }
        DownloadFailure::Io { .. } => t_smol!(translate, "settings_ui.ai.download_failed_io"),
        DownloadFailure::InvalidUrl => t_smol!(translate, "settings_ui.ai.download_failed_url"),
        DownloadFailure::InvalidDestination => {
            t_smol!(translate, "settings_ui.ai.download_failed_destination")
        }
        DownloadFailure::Redirect => {
            t_smol!(translate, "settings_ui.ai.download_failed_redirect")
        }
        DownloadFailure::Task => t_smol!(translate, "settings_ui.ai.download_failed_task"),
    }
}

/// The text a delete failure is drawn with, in the interface's language.
fn delete_failure_reason(translate: &Translate, failure: &DeleteFailure) -> SmolStr {
    match failure {
        DeleteFailure::Unload(UnloadFailure::TimedOut) => {
            t_smol!(translate, "settings_ui.ai.delete_failed_unload_timeout")
        }
        DeleteFailure::Unload(UnloadFailure::Engine) => {
            t_smol!(translate, "settings_ui.ai.delete_failed_unload")
        }
        DeleteFailure::Io { .. } => t_smol!(translate, "settings_ui.ai.delete_failed_io"),
        DeleteFailure::TimedOut => t_smol!(translate, "settings_ui.ai.delete_failed_timeout"),
        DeleteFailure::Provider(_) => t_smol!(translate, "settings_ui.ai.delete_failed_provider"),
    }
}
