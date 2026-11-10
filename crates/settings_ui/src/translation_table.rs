//! The supported translation models, drawn as a table: what each model is, and what the row
//! can do with it.
//!
//! The table is an entity of its own because that is what [`Table`] reads its rows from. It
//! holds one snapshot at a time — the model, whether its file is present and what the download
//! manager last reported for it — and is told to look again when the settings session or the
//! manager changes, which is what makes the rows follow a download as it runs.
//!
//! A row does not download, resume or delete anything itself. A cell is drawn without a window
//! and the page owns the dialogs and the selection, so the row records what the user asked for
//! and the page takes it on the frame it draws — see [`TranslationTable::take_request`]. A pause
//! is the one action with no dialog and no selection behind it, so the table asks the manager
//! for it directly.

use app_config::translation::TranslationModel;
use base_ui::{
    button::Button,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    size::UnifiedSizable,
    style::theme::ThemeExt,
    table::{Table, TableDataSet},
    tooltip::{Tooltip, TooltipExt},
};
use download_provider::error::DownloadFailure;
use gpui_kit::{
    AnyElement, App, Context, ElementId, Empty, Entity, Hsla, InteractiveElement, IntoElement,
    Length, ParentElement, Render, RenderOnce, SharedString, StatefulInteractiveElement, Styled,
    Subscription, WeakEntity, Window,
    base::{Align, Disableable as _},
    div, px, relative,
};
use i18n::t;

use crate::{
    draft::SettingsDraft,
    translation_downloads::{DeleteFailure, DownloadState, TranslationDownloads, UnloadFailure},
};

/// Columns: model name, description, status.
const COLUMN_COUNT: usize = 3;

/// Column widths in header order.
const COLUMN_WIDTHS: [Length; COLUMN_COUNT] = [
    Length::Definite(relative(0.25)),
    Length::Definite(relative(0.33)),
    Length::Definite(relative(0.42)),
];

/// What one row asks the page to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TranslationRequest {
    /// Ask the user which site to download [`TranslationModel`] from, then download it.
    Download { model: TranslationModel },
    /// Continue a paused download from its partial file.
    Resume { model: TranslationModel },
    /// Ask the user which site to retry [`TranslationModel`] from, then retry it.
    Retry { model: TranslationModel },
    /// Remove [`TranslationModel`] and stop naming it where the session did.
    Delete { model: TranslationModel },
}

/// One model's row as the table last read it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ModelRow {
    model: TranslationModel,
    /// Whether the model's file is present in the session's directory.
    downloaded: bool,
    /// What the download manager last reported for this model in that directory.
    state: DownloadState,
}

/// The supported models, drawn as a table.
pub(crate) struct TranslationTable {
    rows: Vec<ModelRow>,
    /// The action a row asked for, which the page takes when it next draws.
    request: Option<TranslationRequest>,
    /// The table's own handle, which its rows are drawn with: a cell is given no window, so a
    /// row cannot reach the entity it belongs to any other way.
    owner: WeakEntity<TranslationTable>,
    /// The session whose directory the rows are about.
    draft: Entity<SettingsDraft>,
    /// The process-wide downloads, when the composition root installed them.
    downloads: Option<Entity<TranslationDownloads>>,
    /// Whether a download can be started: both the manager and a directory have to exist.
    can_download: bool,
    /// Held because the subscription is what keeps the rows of the table the rows of the
    /// session: dropping it would leave a table that never notices the directory changed.
    _draft_changes: Subscription,
    /// Held for the same reason, over the manager the rows report.
    _download_changes: Option<Subscription>,
}

impl TranslationTable {
    pub(crate) fn new(
        draft: Entity<SettingsDraft>,
        downloads: Option<Entity<TranslationDownloads>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let draft_changes = cx.observe(&draft, |table, _, cx| table.reload(cx));
        let download_changes = downloads
            .as_ref()
            .map(|downloads| cx.observe(downloads, |table, _, cx| table.reload(cx)));

        let mut table = Self {
            rows: Vec::new(),
            request: None,
            owner: cx.entity().downgrade(),
            draft,
            downloads,
            can_download: false,
            _draft_changes: draft_changes,
            _download_changes: download_changes,
        };
        table.reload(cx);

        table
    }

    /// Takes the action a row asked for, so the page opens it once.
    pub(crate) fn take_request(&mut self) -> Option<TranslationRequest> {
        self.request.take()
    }

    /// Whether the table offers the download action, which a test reads to pin what the page
    /// draws without the manager or the directory.
    #[cfg(test)]
    pub(crate) fn downloads_enabled(&self) -> bool {
        self.can_download
    }

    /// Records the action the user asked for, which the page takes when it next draws.
    fn request(&mut self, request: TranslationRequest, cx: &mut Context<Self>) {
        self.request = Some(request);
        cx.notify();
    }

    /// Stops the download of `model` in the directory the session holds.
    fn pause(&mut self, model: TranslationModel, cx: &mut Context<Self>) {
        let Some(directory) = self
            .draft
            .read(cx)
            .config()
            .translation
            .model_directory
            .clone()
        else {
            return;
        };
        let Some(downloads) = self.downloads.clone() else {
            return;
        };
        downloads.update(cx, |downloads, cx| {
            downloads.pause(model, directory, cx);
        });
    }

    /// Re-reads the rows of the session and the manager, and tells the table to draw them again
    /// when they changed.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let config = self.draft.read(cx).config();
        let directory = config.translation.model_directory.clone();
        drop(config);

        let (rows, can_download) = match (self.downloads.as_ref(), directory.as_deref()) {
            (Some(downloads), Some(directory)) => {
                let downloads = downloads.read(cx);
                let rows = TranslationModel::ALL
                    .into_iter()
                    .map(|model| {
                        let state = downloads.row_state(model, directory);
                        // A transfer in progress says whether the file is there: its state is
                        // what the row draws, so only an idle row pays for looking at the disk.
                        let downloaded = matches!(state, DownloadState::Idle)
                            && downloads.is_downloaded(model, directory);
                        ModelRow {
                            model,
                            downloaded,
                            state,
                        }
                    })
                    .collect();
                (rows, true)
            }
            _ => (
                TranslationModel::ALL
                    .into_iter()
                    .map(|model| ModelRow {
                        model,
                        downloaded: false,
                        state: DownloadState::Idle,
                    })
                    .collect(),
                false,
            ),
        };

        let changed = self.rows != rows || self.can_download != can_download;
        self.rows = rows;
        self.can_download = can_download;
        if changed {
            cx.notify();
        }
    }
}

impl TableDataSet<COLUMN_COUNT> for TranslationTable {
    fn rows_count(&self) -> usize {
        self.rows.len()
    }

    fn headers(&self, cx: &App) -> [SharedString; COLUMN_COUNT] {
        [
            t!(cx, "settings_ui.ai.model_name"),
            t!(cx, "settings_ui.ai.description"),
            t!(cx, "settings_ui.ai.status"),
        ]
    }

    fn column_aligns(&self) -> [Align; COLUMN_COUNT] {
        [Align::Start; COLUMN_COUNT]
    }

    fn column_widths(&self) -> [Length; COLUMN_COUNT] {
        COLUMN_WIDTHS
    }

    /// The description and the status text speak in sentences: a narrower table wraps them onto
    /// another line instead of hiding what they say behind an ellipsis. The model name stays one
    /// line, and its tooltip keeps the whole of it readable when even that is cut.
    fn cell_wraps(&self, _row_index: usize, column_index: usize) -> bool {
        matches!(column_index, 1 | 2)
    }

    fn cell(&self, row_index: usize, column_index: usize, _cx: &App) -> impl IntoElement {
        let Some(row) = self.rows.get(row_index) else {
            return Empty.into_any_element();
        };

        TranslationCell {
            row: row.clone(),
            column: column_index,
            can_download: self.can_download,
            owner: self.owner.clone(),
        }
        .into_any_element()
    }
}

/// One cell of one row, drawn when the table asks for it.
#[derive(IntoElement)]
struct TranslationCell {
    row: ModelRow,
    column: usize,
    can_download: bool,
    owner: WeakEntity<TranslationTable>,
}

impl RenderOnce for TranslationCell {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            row,
            column,
            can_download,
            owner,
        } = self;
        let index = TranslationModel::ALL
            .iter()
            .position(|model| *model == row.model)
            .unwrap_or_default();

        match column {
            0 => {
                let name = SharedString::from(row.model.display_name());
                Tooltip::new(
                    ElementId::NamedInteger(
                        SharedString::new_static("settings-ai-model-name-tooltip"),
                        index as u64,
                    ),
                    div()
                        .debug_selector(move || format!("SETTINGS_AI_MODEL_{index}"))
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .child(name.clone()),
                )
                .content(name)
                .into_any_element()
            }
            1 => {
                let description = t!(
                    cx,
                    "settings_ui.ai.model_description",
                    size = row.model.parameter_size(),
                    bits = row.model.quantization_bits()
                );
                Tooltip::new(
                    ElementId::NamedInteger(
                        SharedString::new_static("settings-ai-model-description-tooltip"),
                        index as u64,
                    ),
                    div()
                        .debug_selector(move || format!("SETTINGS_AI_MODEL_DESCRIPTION_{index}"))
                        .w_full()
                        .min_w_0()
                        .whitespace_normal()
                        .line_height(relative(1.2))
                        .text_xs()
                        .child(description.clone()),
                )
                .content(description)
                .into_any_element()
            }
            2 => status_cell(&row, index, can_download, &owner, cx),
            _ => Empty.into_any_element(),
        }
    }
}

/// The actions and the text of one row, in the order a reader meets them.
fn status_cell(
    row: &ModelRow,
    index: usize,
    can_download: bool,
    owner: &WeakEntity<TranslationTable>,
    cx: &App,
) -> AnyElement {
    let model = row.model;
    let mut status = div().w_full().flex().items_center().gap_1().min_w_0();
    let mut text: Option<AnyElement> = None;
    let action = |action: RowAction, disabled: bool| {
        action_button(action, index, model, owner, disabled, cx)
    };

    match &row.state {
        DownloadState::Deleting => {
            status = status.child(action(RowAction::Deleting, true));
        }
        DownloadState::Downloading { downloaded } => {
            status = status
                .child(action(RowAction::Pause, false))
                .child(action(RowAction::Delete, false));
            text = Some(progress_text(
                index,
                t!(
                    cx,
                    "settings_ui.ai.downloading_progress",
                    percent = whole_percent(model, *downloaded)
                ),
                cx,
            ));
        }
        DownloadState::Pausing { .. } => {
            status = status
                .child(action(RowAction::Pause, true))
                .child(action(RowAction::Delete, false));
            text = Some(progress_text(index, t!(cx, "settings_ui.ai.pausing"), cx));
        }
        DownloadState::Paused { downloaded } => {
            // A paused download has no delete beside it: continuing it is the only thing the
            // row offers, and discarding it is the failed download's row to offer.
            status = status.child(action(RowAction::Resume, false));
            text = Some(progress_text(
                index,
                t!(
                    cx,
                    "settings_ui.ai.paused_progress",
                    percent = whole_percent(model, *downloaded)
                ),
                cx,
            ));
        }
        DownloadState::DownloadFailed { failure } => {
            let reason = download_failure_reason(failure, cx);
            status = status
                .child(action(RowAction::Retry, false))
                .child(action(RowAction::Delete, false));
            text = Some(failure_text(
                index,
                t!(cx, "settings_ui.ai.download_failed", reason = reason),
                cx,
            ));
        }
        DownloadState::DeleteFailed { failure } => {
            let reason = delete_failure_reason(failure, cx);
            status = status.child(action(RowAction::Delete, false));
            text = Some(failure_text(
                index,
                t!(cx, "settings_ui.ai.delete_failed", reason = reason),
                cx,
            ));
        }
        DownloadState::Idle => {
            if row.downloaded {
                status = status.child(action(RowAction::Delete, false));
            } else {
                status = status.child(action(RowAction::Download, !can_download));
            }
        }
    }

    if let Some(text) = text {
        status = status.child(text);
    }

    status.into_any_element()
}

/// The icon-only action one row draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowAction {
    Download,
    Pause,
    Resume,
    Retry,
    Delete,
    /// The disabled bin a row is drawn with while its delete is in flight.
    Deleting,
}

impl RowAction {
    fn label_key(self) -> &'static str {
        match self {
            Self::Download => "settings_ui.ai.download",
            Self::Pause => "settings_ui.ai.pause",
            Self::Resume => "settings_ui.ai.resume",
            Self::Retry => "settings_ui.ai.retry",
            Self::Delete => "settings_ui.ai.delete",
            Self::Deleting => "settings_ui.ai.deleting",
        }
    }

    fn icon(self) -> IconKind {
        match self {
            Self::Download => IconKind::DownloadSimple,
            Self::Pause => IconKind::Pause,
            Self::Resume => IconKind::Play,
            Self::Retry => IconKind::ArrowsClockwise,
            Self::Delete | Self::Deleting => IconKind::Trash,
        }
    }

    /// The identity one action's button is drawn under.
    fn id_prefix(self) -> &'static str {
        match self {
            Self::Download => "settings-ai-model-download",
            Self::Pause => "settings-ai-model-pause",
            Self::Resume => "settings-ai-model-resume",
            Self::Retry => "settings-ai-model-retry",
            Self::Delete => "settings-ai-model-delete",
            Self::Deleting => "settings-ai-model-deleting",
        }
    }

    /// What one action's button is drawn under, which is what a test clicks it by.
    fn debug_prefix(self) -> &'static str {
        match self {
            Self::Download => "SETTINGS_AI_MODEL_DOWNLOAD",
            Self::Pause => "SETTINGS_AI_MODEL_PAUSE",
            Self::Resume => "SETTINGS_AI_MODEL_RESUME",
            Self::Retry => "SETTINGS_AI_MODEL_RETRY",
            Self::Delete => "SETTINGS_AI_MODEL_DELETE",
            Self::Deleting => "SETTINGS_AI_MODEL_DELETING",
        }
    }

    /// What the page is asked for, for the actions that outlive the frame the click was in.
    fn request(self, model: TranslationModel) -> Option<TranslationRequest> {
        Some(match self {
            Self::Download => TranslationRequest::Download { model },
            Self::Resume => TranslationRequest::Resume { model },
            Self::Retry => TranslationRequest::Retry { model },
            Self::Delete => TranslationRequest::Delete { model },
            Self::Pause | Self::Deleting => return None,
        })
    }
}

/// One icon-only action button, asking the table for the action or pausing it directly.
fn action_button(
    action: RowAction,
    index: usize,
    model: TranslationModel,
    owner: &WeakEntity<TranslationTable>,
    disabled: bool,
    cx: &App,
) -> Button {
    let label = t!(cx, action.label_key());
    let danger = matches!(action, RowAction::Delete | RowAction::Deleting);
    let mut icon = Icon::new(action.icon()).size_3();
    if danger {
        icon = icon.text_color(cx.theme().roles.status.danger.base.hsla);
    }

    let mut button = Button::new(ElementId::NamedInteger(
        SharedString::new_static(action.id_prefix()),
        index as u64,
    ))
    .small()
    .ghost()
    .flex_none()
    .disabled(disabled)
    .debug_selector(move || format!("{}_{index}", action.debug_prefix()))
    .aria_label(label.clone())
    .with_tooltip(label)
    .child(icon);

    let owner = owner.clone();
    if disabled {
        return button;
    }

    match action {
        RowAction::Pause => {
            button = button.on_click(move |_, _, cx| {
                let Some(table) = owner.upgrade() else {
                    return;
                };
                table.update(cx, |table, cx| table.pause(model, cx));
            });
        }
        RowAction::Deleting => {}
        _ => {
            let request = action
                .request(model)
                .expect("every other action is one the page takes");
            button = button.on_click(move |_, _, cx| {
                let Some(table) = owner.upgrade() else {
                    return;
                };
                table.update(cx, |table, cx| table.request(request.clone(), cx));
            });
        }
    }

    button
}

/// The text a running or paused download is drawn with, beside its buttons.
fn progress_text(index: usize, caption: SharedString, cx: &App) -> AnyElement {
    status_text(
        "SETTINGS_AI_MODEL_PROGRESS",
        index,
        caption,
        cx.theme().roles.fg.muted.hsla,
    )
}

/// The text a failure is drawn with, beside its buttons.
fn failure_text(index: usize, caption: SharedString, cx: &App) -> AnyElement {
    status_text(
        "SETTINGS_AI_MODEL_ERROR",
        index,
        caption,
        cx.theme().roles.status.danger.base.hsla,
    )
}

/// The line of status text beside the buttons of one row: small, and wrapped inside the column
/// it is in so a long reason stays readable.
fn status_text(
    prefix: &'static str,
    index: usize,
    caption: SharedString,
    color: Hsla,
) -> AnyElement {
    let full_text = caption.clone();
    div()
        .flex_1()
        .min_w_0()
        .child(
            Tooltip::new(
                ElementId::NamedInteger(
                    SharedString::new_static("settings-ai-model-status-tooltip"),
                    index as u64,
                ),
                div()
                    .debug_selector(move || format!("{prefix}_{index}"))
                    .w_full()
                    .min_w_0()
                    .whitespace_normal()
                    .line_height(relative(1.2))
                    .text_xs()
                    .text_color(color)
                    .child(caption),
            )
            .content(full_text),
        )
        .into_any_element()
}

/// How much of `model`'s published file `downloaded` bytes are, as a whole percentage.
fn whole_percent(model: TranslationModel, downloaded: u64) -> u32 {
    let total = model.size_bytes();
    if total == 0 {
        return 0;
    }

    ((downloaded as f64 / total as f64) * 100.0)
        .clamp(0.0, 100.0)
        .round() as u32
}

/// The text a download failure is drawn with, in the interface's language.
fn download_failure_reason(failure: &DownloadFailure, cx: &App) -> SharedString {
    match failure {
        DownloadFailure::Status { status } => {
            t!(cx, "settings_ui.ai.download_failed_status", status = status)
        }
        // The transport that could not be started, the request that could not be sent and the
        // body that could not be read are one message to the reader: the download failed on
        // the network.
        DownloadFailure::Transport | DownloadFailure::Request | DownloadFailure::Read => {
            t!(cx, "settings_ui.ai.download_failed_transport")
        }
        DownloadFailure::Route => t!(cx, "settings_ui.ai.download_failed_route"),
        DownloadFailure::Range => t!(cx, "settings_ui.ai.download_failed_range"),
        DownloadFailure::SizeMismatch { expected, actual } => t!(
            cx,
            "settings_ui.ai.download_failed_size",
            expected = expected,
            actual = actual
        ),
        DownloadFailure::TooLarge { .. } => t!(cx, "settings_ui.ai.download_failed_too_large"),
        DownloadFailure::Cancelled => t!(cx, "settings_ui.ai.download_failed_cancelled"),
        DownloadFailure::Io { .. } => t!(cx, "settings_ui.ai.download_failed_io"),
        DownloadFailure::InvalidUrl => t!(cx, "settings_ui.ai.download_failed_url"),
        DownloadFailure::InvalidDestination => t!(cx, "settings_ui.ai.download_failed_destination"),
        DownloadFailure::Redirect => t!(cx, "settings_ui.ai.download_failed_redirect"),
        DownloadFailure::Task => t!(cx, "settings_ui.ai.download_failed_task"),
    }
}

/// The text a delete failure is drawn with, in the interface's language.
fn delete_failure_reason(failure: &DeleteFailure, cx: &App) -> SharedString {
    match failure {
        DeleteFailure::Unload(UnloadFailure::TimedOut) => {
            t!(cx, "settings_ui.ai.delete_failed_unload_timeout")
        }
        DeleteFailure::Unload(UnloadFailure::Engine) => {
            t!(cx, "settings_ui.ai.delete_failed_unload")
        }
        DeleteFailure::Io { .. } => t!(cx, "settings_ui.ai.delete_failed_io"),
        DeleteFailure::TimedOut => t!(cx, "settings_ui.ai.delete_failed_timeout"),
        DeleteFailure::Provider(_) => t!(cx, "settings_ui.ai.delete_failed_provider"),
    }
}

impl Render for TranslationTable {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let table = cx.entity();

        div()
            .debug_selector(|| "SETTINGS_AI_MODEL_TABLE".into())
            .w_full()
            .flex_1()
            .min_h_0()
            .rounded_sm()
            .border_1()
            .border_color(theme.roles.line.divider.hsla)
            .bg(theme.roles.surface.sunken.hsla)
            .overflow_hidden()
            .child(Table::new("settings-ai-model-table", &table).row_height(px(48.0)))
    }
}

#[cfg(test)]
mod tests {
    use app_config::{settings::Config, translation::TranslationModel};
    use base_ui::{icon::IconKind, table::TableDataSet as _};
    use gpui_kit::{AppContext as _, TestAppContext};
    use i18n::{global::embedded, language::codes::LanguageCode, t};

    use super::{RowAction, TranslationTable, whole_percent};
    use crate::draft::SettingsDraft;

    /// The label and the icon of every row action, which are the copy and the picture the
    /// status column was designed with.
    #[test]
    fn every_row_action_should_keep_its_copy_and_its_icon() {
        let actions = [
            (
                RowAction::Download,
                "settings_ui.ai.download",
                IconKind::DownloadSimple,
            ),
            (RowAction::Pause, "settings_ui.ai.pause", IconKind::Pause),
            (RowAction::Resume, "settings_ui.ai.resume", IconKind::Play),
            (
                RowAction::Retry,
                "settings_ui.ai.retry",
                IconKind::ArrowsClockwise,
            ),
            (RowAction::Delete, "settings_ui.ai.delete", IconKind::Trash),
            (
                RowAction::Deleting,
                "settings_ui.ai.deleting",
                IconKind::Trash,
            ),
        ];

        for (action, label_key, icon) in actions {
            assert_eq!(action.label_key(), label_key);
            assert_eq!(action.icon(), icon);
        }
    }

    /// A normal model's published file is what the progress percentage is a share of: none of
    /// it is zero, all of it is a hundred, and bytes beyond it are still a hundred.
    #[test]
    fn whole_percent_should_measure_a_download_against_the_published_file() {
        let model = TranslationModel::HyMt218BQ4Km;
        let total = model.size_bytes();

        assert_eq!(whole_percent(model, 0), 0, "nothing downloaded is zero");
        assert_eq!(
            whole_percent(model, total / 2),
            50,
            "half the file is fifty percent"
        );
        assert_eq!(whole_percent(model, total), 100, "the whole file is full");
        assert_eq!(
            whole_percent(model, total + 1),
            100,
            "a task that reports more than the published size is clamped"
        );
        assert_eq!(
            whole_percent(model, u64::MAX),
            100,
            "the clamp also holds when the share overflows a percentage"
        );
    }

    /// The columns that speak in sentences wrap onto another line; the model name keeps the one
    /// line it has always been drawn with.
    #[gpui_kit::test]
    fn only_the_description_and_status_columns_should_wrap(cx: &mut TestAppContext) {
        let draft = cx.update(|cx| cx.new(|_| SettingsDraft::new(Config::default())));
        let table = cx.update(|cx| cx.new(|cx| TranslationTable::new(draft, None, cx)));

        let wraps = cx.read(|cx| {
            let table = table.read(cx);
            (0..3)
                .map(|column| table.cell_wraps(0, column))
                .collect::<Vec<_>>()
        });

        assert_eq!(
            wraps,
            [false, true, true],
            "the model name is cut to one line while the description and status wrap"
        );
    }

    /// The description under a model's name is the sentence that says what the file is, and
    /// both languages say exactly that.
    #[gpui_kit::test]
    fn the_model_description_should_name_the_size_and_bits_in_both_languages(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            embedded::install(cx).expect("embedded locales should load");

            assert_eq!(
                t!(
                    cx,
                    "settings_ui.ai.model_description",
                    size = "1.8B",
                    bits = "1.25"
                )
                .as_ref(),
                "1.25-bit quantized version of the HY-MT2 1.8B model",
                "the English description names the model and the quantization"
            );

            cx.set_global(LanguageCode::ZhCn);
            assert_eq!(
                t!(
                    cx,
                    "settings_ui.ai.model_description",
                    size = "1.8B",
                    bits = "1.25"
                )
                .as_ref(),
                "HY-MT2 1.8B模型的1.25Bit量化版本",
                "the Chinese description is the sentence the page was designed with"
            );
        });
    }
}
