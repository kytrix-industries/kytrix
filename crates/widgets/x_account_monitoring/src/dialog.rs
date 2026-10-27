//! The dialog a pane opens to manage the accounts its timeline is read from.
//!
//! # Two pages, one list of accounts
//!
//! The dialog shows the accounts being monitored and, behind a control of its own, a search of
//! X for one more. The two pages draw different state. What is monitored, what the last change
//! said and which request is outstanding belong to the pane, because the service answering them
//! is the pane's and the accounts outlive any one dialog: a dialog that held them would lose
//! them on the next open. The search belongs to the dialog: the query is typed in its field and
//! the request is made for it, so what a search found is the page's own and goes when the dialog
//! goes — which is the point, because an answer drawn under a query a later dialog never asked
//! is worse than no answer at all.
//!
//! # The pages are a navigation stack
//!
//! The two pages are pushed onto and popped off a [`NavStack`], so that going to the search
//! and coming back is the push and pop of a router rather than a control that redraws the
//! dialog in place: each page is a view of its own — its own header, its own field and its own
//! list — which is what lets one slide in over the other and what keeps a page's place in its
//! list while the other is on top. The stack is opened on the accounts, and the control that
//! searches X pushes the page that does it while the control in that page's own header pops it.
//!
//! # A control that can only fail is disabled
//!
//! An account already monitored, a search that would be the one past the quota, a page with no
//! accounts to add to: each of those is a control the dialog disables rather than sends, and
//! the state that decides it is the same state the service would refuse with. The pane is what
//! refuses it a second time, because a field that answers Enter is a way in without a control
//! to have been disabled.
//!
//! # What a search says is drawn on the page that asked
//!
//! A search is a procedure of the page that runs it rather than a change to the accounts, so
//! neither a search being out nor one that could not be answered is an alert over the dialog:
//! the page draws its own spinner while one is out, and the words the failure itself gives where
//! it was asked for. An alert is for what a change to the accounts said, which is not something
//! the page that asked for it can draw — and a failure a reader looks for in the place they
//! asked is not a failure to be dismissed before they can read it.
//!
//! # What a change says is reported in an alert
//!
//! An account that joined the list and a change that was refused are the two things a change
//! has to say, and either of them is one alert over the dialog with the one button that says it
//! was read: a service's answer is not a line of a page, which a reader looks past. What the
//! alert is raised for is the report the pane is holding and not the frames it is held for, so
//! a change is one alert; a change the dialog is not there to hear — it was dismissed before
//! the answer arrived, or another dialog heard it — is a change with nowhere to be reported,
//! and what a dialog reports is what a change says while it is there to say it.

use std::{sync::Arc, time::Duration};

use ahash::AHashMap as HashMap;
use base_ui::{
    badge::Badge,
    button::Button,
    icon::{Icon, IconKind},
    input::{Input, InputState, InputStateBuilder, InputStateEvent},
    interactive::{Interactive, InteractiveVariantExt},
    layer::LayerExt,
    markdown::view::{Markdown, MarkdownState},
    separator::Separator,
    size::UnifiedSizable,
    spinner::Spinner,
    static_sized_list::StaticSizedList,
    style::theme::ThemeExt,
    tooltip::TooltipExt,
};
use gpui_kit::{
    AnyElement, App, AppContext, Context, DismissEvent, ElementId, Entity, EventEmitter,
    FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, Render, Role, SharedString,
    StatefulInteractiveElement, Styled, WeakEntity, Window,
    base::{
        Disableable, NavMotion, NavOperation, NavPage, NavStack, NavStackState,
        motion::{PresencePhase, Transition},
    },
    div,
    prelude::FluentBuilder,
    px, relative,
};
use i18n::t;
use kytrix_service_provider::service::x::{
    XAccountMonitoringApiError, XAccountMonitoringProfile, XAccountMonitoringSelection,
};
use smol_str::{SmolStr, format_smolstr};
use widget::{error::LocalizedError, rpc::RpcCallError, status::WidgetAction};

use super::{
    model::{MediaStates, SelectionData, selection_name},
    view::{
        SelectionNotice, SelectionOperation, XAccountMonitoringWidget, avatar, centered_message,
        message_with_action, recovery_button, status_label, valid_search_query,
    },
};

/// The size of the dialog's panel.
const DIALOG_WIDTH: f32 = 580.0;
const DIALOG_HEIGHT: f32 = 640.0;

/// How long one page takes to slide in over another, or back out from under it.
pub(super) const PAGE_TRANSITION: Duration = Duration::from_millis(220);

/// How far the page underneath drifts while the page over it slides, as a fraction of the width
/// of the panel.
///
/// A page that is being covered moves a little the other way rather than staying where it is,
/// which is what reads as one page lying under another rather than two pages swapping places.
const PAGE_DEPTH_DRIFT: f32 = 0.3;

/// The list the dialog draws the monitored accounts with.
const ACCOUNT_LIST_ID: &str = "tweet-monitor-account-list";

/// The list the dialog draws search results with.
const RESULT_LIST_ID: &str = "tweet-monitor-result-list";

/// The field the monitored accounts are filtered by, which filters nothing but what is on
/// screen: the service is never asked about it.
const FILTER_INPUT_ID: &str = "tweet-monitor-filter";

/// The field X is searched with.
const QUERY_INPUT_ID: &str = "tweet-monitor-query";

/// The size of the face one row of either list is drawn with.
const ROW_AVATAR_SIZE: f32 = 42.0;

/// The height one row of either list is drawn at.
///
/// A list of places measures the first row it draws and lays every other one out at that
/// height, so what a row is worth has to be the row's own height rather than whatever its
/// content asks for: the row is given one here, which is the face, the lines beside it and a
/// little room around them. A control's own size is not one — a control has the height of a
/// control, and a row drawn at it would be laid out at that height and overflow it.
const ROW_HEIGHT: f32 = 72.0;

/// The place one monitored account is drawn, which is what a test measures a row by. The rows
/// of a list are all one size, so the name is the same for every one of them and the last row
/// drawn is the one that answers.
const ACCOUNT_ROW_SELECTOR: &str = "TEST_TWEET_ACCOUNT_ROW";

/// The place one search result is drawn, on the same terms.
const SEARCH_ROW_SELECTOR: &str = "TEST_TWEET_SEARCH_ROW";

/// The face one monitored account is drawn with, which is the tallest thing its row holds.
const ACCOUNT_AVATAR_SELECTOR: &str = "TEST_TWEET_ACCOUNT_AVATAR";

/// The face one search result is drawn with.
const SEARCH_AVATAR_SELECTOR: &str = "TEST_TWEET_SEARCH_AVATAR";

/// Which of the dialog's two pages a view is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DialogPage {
    /// The accounts being monitored, which is the page the stack opens on.
    Monitored,
    /// A search of X for one more.
    Add,
}

/// What one search of X answered.
///
/// A search is a procedure of the page that runs it rather than a change to the accounts, so
/// what it answered is that page's state rather than the pane's: it is handed to the dialog that
/// asked for it and dies with it. "Found nothing" is [`SearchAnswer::Found`] with no profile in
/// it, which is an answer; a query nothing has been searched with is not one at all, and is the
/// page holding no answer.
pub(super) enum SearchAnswer {
    /// The accounts the search found, which may be none of them.
    Found(Arc<[XAccountMonitoringProfile]>),
    /// Why the search could not be answered.
    Failed(RpcCallError<XAccountMonitoringApiError>),
}

/// One dialog over a pane.
///
/// It owns what a pane has no use for — what the user is typing, the search that page runs and
/// what came of it, the two pages with the stack between them, the two lists with the place a
/// reader is in each of them, and the document the body's own sentence is drawn from — and draws
/// everything else from the pane, which is what holds the service that answers a read or a change.
pub(super) struct TweetDialog {
    widget: WeakEntity<XAccountMonitoringWidget>,
    /// The pages, of which the top one is showing.
    stack: Entity<NavStackState>,
    /// The page X is searched for one more account on, which is the one pushed over the
    /// accounts and the one the control in its own header pops again.
    add: Entity<PageView>,
    /// The field the monitored accounts are filtered by.
    filter: Entity<InputState>,
    /// The field X is searched with.
    query: Entity<InputState>,
    /// What the filter holds.
    ///
    /// The input element is rebuilt every frame and its state exposes no value, so what is
    /// typed is followed through the events the input emits rather than read back off it.
    typed_filter: Option<SmolStr>,
    typed_query: Option<SmolStr>,
    /// What the last search answered, or `None` when there is nothing to draw for one.
    ///
    /// The query is what asks for a search, so an answer stands for the query it was asked with
    /// and for no other: editing the query takes the answer away rather than leaving it under a
    /// query it does not stand for.
    search: Option<SearchAnswer>,
    /// The accounts the account monitors, of the ones the filter shows.
    accounts: Entity<StaticSizedList<XAccountMonitoringSelection>>,
    /// The pane's rows the monitored list was last told to draw, and which of them those were.
    shown_accounts: Option<(Arc<[XAccountMonitoringSelection]>, Vec<usize>)>,
    /// The accounts one search found.
    results: Entity<StaticSizedList<XAccountMonitoringProfile>>,
    /// What the search list was last told to draw.
    shown_results: Option<Arc<[XAccountMonitoringProfile]>>,
    /// What the last change said the last time the dialog reported it.
    ///
    /// The pane holds what a change said until another change replaces it, so the same report
    /// is read from it on every frame of the alert it raised: this is the record of the report
    /// already made, and `None` is the pane holding nothing to report.
    ///
    /// A dialog starts out holding whatever the pane was already holding, because that report
    /// was not made to it: it was made to another dialog, or to none at all — a change that
    /// landed while none was open. Either way the change is one this dialog was not there to
    /// hear, and reporting it would raise an alert about something the reader did in a session
    /// that is over.
    reported: Option<SelectionNotice>,
    /// The document the list's own sentence is drawn from when there is nothing to list.
    ///
    /// The sentence is catalogue copy — the same copy the pane's placeholder draws — and the
    /// sentences of the catalogue are written as markdown, so it is drawn by the markdown
    /// component here too. The document lives with the dialog rather than being built per frame,
    /// and a frame that resolves a different sentence is the one that re-parses it: a language
    /// the interface moved to since is a different sentence, so that frame catches it as well.
    message: Entity<MarkdownState>,
}

/// The dialog is what the layer hosts, so it is what says it is done: closing is an event of its
/// own rather than a call on the window, which is what lets its footer close it without a window
/// and without depending on being the dialog on top.
impl EventEmitter<DismissEvent> for TweetDialog {}

impl TweetDialog {
    /// Opens the dialog over the window, for the pane the control belongs to.
    ///
    /// Nothing happens when it is already open: the control is the dialog's only entrance, and
    /// replacing the open one would throw away what the user has typed.
    pub(super) fn show(
        widget: WeakEntity<XAccountMonitoringWidget>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(pane) = widget.upgrade() else {
            return;
        };
        if pane.read(cx).dialog_open() {
            return;
        }
        let filter = cx.new(|cx| {
            InputStateBuilder::new(FILTER_INPUT_ID)
                .placeholder(t!(cx, "widget.x_account_monitoring.filter_placeholder"))
                .build(window, cx)
        });
        let query = cx.new(|cx| {
            InputStateBuilder::new(QUERY_INPUT_ID)
                .placeholder(t!(cx, "widget.x_account_monitoring.search_placeholder"))
                .build(window, cx)
        });
        let dialog = cx.new(|cx| Self::new(widget.clone(), filter, query, cx));

        // The pane's handle on the dialog is taken here, before the dialog is opened, and this is
        // the order that matters: `window.dialog(cx, dialog)` moves the entity onto the layer, so
        // it is `dialog` itself that is handed over and the only handle this frame keeps is the
        // weak one below. A strong handle left behind here would be a dialog that outlives the
        // layer drawing it — and a pane that never hears it is gone.
        let opened = dialog.downgrade();

        // Dismissing the dialog is what stops the pane asking for the avatars it was drawing:
        // the entity is released once the layer has stopped drawing it, and nothing else tells the
        // pane it is gone.
        //
        // The release is what this hangs on rather than the close signal the dialog itself listens
        // for. The two part company at the end of a dialog's life: the signal arrives while the
        // dialog is still drawn, fading out, and this cleanup has to run when the avatars have
        // stopped being drawn rather than while they still are. What the pane is letting go of is
        // the dialog, and the release is when there is no dialog left to let go of.
        cx.observe_release(&dialog, |dialog: &mut Self, cx| {
            let Some(pane) = dialog.widget.upgrade() else {
                return;
            };
            pane.update(cx, |pane, cx| pane.on_dialog_closed(cx));
        })
        .detach();

        window.dialog(cx, dialog);

        // What the pane holds is weak, so the layer is the only thing keeping the dialog alive now.
        // Handing `dialog` over does not open a window in which the entity is released early: the
        // layer defers its push, the deferred call holds a strong reference of its own until it has
        // run, and the layer's own reference is taken before that one is dropped. So the release the
        // observer above listens for is the layer ceasing to draw the dialog, and nothing else.
        pane.update(cx, |pane, cx| pane.attach_dialog(opened, cx));
    }

    pub(super) fn new(
        widget: WeakEntity<XAccountMonitoringWidget>,
        filter: Entity<InputState>,
        query: Entity<InputState>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.subscribe(&filter, |dialog, _, event: &InputStateEvent, cx| {
            if let InputStateEvent::Change(value) = event {
                let typed = (!value.is_empty()).then(|| SmolStr::new(value.as_str()));
                if dialog.typed_filter != typed {
                    dialog.typed_filter = typed;
                    // A filter is how far down a list a reader is, and another filter is
                    // another place in it: the accounts the list is given change with the
                    // filter, and being given them is what puts the reader back at the top.
                    cx.notify();
                }
            }
        })
        .detach();
        cx.subscribe(
            &query,
            |dialog, _, event: &InputStateEvent, cx| match event {
                InputStateEvent::Change(value) => {
                    let typed = (!value.is_empty()).then(|| SmolStr::new(value.as_str()));
                    if dialog.typed_query != typed {
                        dialog.typed_query = typed;
                        // What a search found belongs to the query it was asked with, so editing
                        // the query takes the answer away rather than leaving it under a query it
                        // does not stand for.
                        dialog.search = None;
                        cx.notify();
                    }
                }
                InputStateEvent::PressEnter(_) => dialog.search(cx),
                // Nothing here commits on a blur: these fields are searched as they are
                // typed, so there is nothing left to submit when the caret leaves.
                InputStateEvent::Blur(_) => {}
            },
        )
        .detach();

        // Each list is built once and told what to draw, so the rows it draws are given the
        // pane and the dialog rather than the state of the frame they are drawn in: what a row
        // reads is the pane the dialog reads, which is what holds the accounts and what is
        // being fetched for them.
        let pane = widget.clone();
        let dialog = cx.weak_entity();
        let rows = dialog.clone();
        let accounts = cx.new(|_| {
            StaticSizedList::new(
                ElementId::Name(SharedString::new_static(ACCOUNT_LIST_ID)),
                {
                    let pane = pane.clone();
                    let dialog = rows.clone();
                    move |_index, selection: &XAccountMonitoringSelection, _window, cx| {
                        account_row(selection, &pane, &dialog, cx)
                    }
                },
            )
        });
        let results = cx.new(|_| {
            StaticSizedList::new(
                ElementId::Name(SharedString::new_static(RESULT_LIST_ID)),
                move |_index, profile: &XAccountMonitoringProfile, _window, cx| {
                    search_result_row(profile, &pane, &rows, cx)
                },
            )
        });

        // The pages are built once and pushed onto the stack, so what a page holds — the caret
        // in its field and the place a reader is in its list — is the page's own and outlives
        // the other one being shown over it. The stack opens on the accounts, which is what the
        // dialog is for; the search of X is a page pushed over them, so the page the dialog
        // holds on to is that one — the accounts are what the stack is left holding without it.
        let stack = cx.new(|_| NavStackState::new());
        let add = cx.new(|_| PageView::new(widget.clone(), dialog.clone(), DialogPage::Add));
        let root = cx.new(|_| PageView::new(widget.clone(), dialog, DialogPage::Monitored));
        stack.update(cx, |stack, cx| stack.push(root, NavMotion::Immediate, cx));
        // A page puts the other one on the stack itself, but which page is on top is what
        // decides how the dialog draws its frame — and what the pages draw is what the dialog
        // is drawing them in.
        cx.observe(&stack, |_, _, cx| cx.notify()).detach();

        // What the pane is already holding was reported to a dialog that is gone, or to none at
        // all, so this one starts out having said it: what it reports is what a change says
        // while it is there to say it.
        let reported = widget
            .upgrade()
            .and_then(|pane| pane.read(cx).dialog_state().notice);

        Self {
            widget,
            stack,
            add,
            filter,
            query,
            typed_filter: None,
            typed_query: None,
            search: None,
            accounts,
            shown_accounts: None,
            results,
            shown_results: None,
            reported,
            // The document starts empty: the sentence a list with nothing to draw says depends on
            // the pane, which the first frame reads and resolves like every frame after it.
            message: cx.new(|cx| MarkdownState::new("", cx)),
        }
    }

    /// Reads the list's sentence into this dialog's document, if it is not the one already there.
    ///
    /// `set_source` is what decides that: a sentence that has not moved is left alone, so a frame
    /// spends no parse on it — and a sentence that has moved, because the pane changed or the
    /// language did, is parsed exactly once. The document is drawn under the branch
    /// [`Self::list`] takes instead of that sentence, so what it holds while there is a list to
    /// draw costs nothing.
    fn resolve_message(&mut self, pane: &Entity<XAccountMonitoringWidget>, cx: &mut Context<Self>) {
        let state = pane.read(cx).dialog_state();
        let Some(message) = self.message_text(&state, cx) else {
            return;
        };

        self.message
            .update(cx, |document, cx| document.set_source(message, cx));
    }

    /// The sentence that stands in for the list, when there is nothing to draw in it.
    ///
    /// Which sentence it is is asked of the same state the list itself is drawn from, so the two
    /// cannot disagree about whether there is anything to show.
    fn message_text(&self, state: &DialogState, cx: &App) -> Option<SharedString> {
        if !state.signed_in {
            return Some(t!(cx, "widget.x_account_monitoring.logged_out"));
        }
        let data = state.selections.as_ref()?;
        if data.selections().is_empty() {
            return Some(t!(cx, "widget.x_account_monitoring.empty_accounts"));
        }

        filtered_selection_indices(
            data.selections(),
            &state.hints,
            self.typed_filter.as_deref(),
        )
        .is_empty()
        .then(|| t!(cx, "common.no_matches"))
    }

    /// Shows the page the search of X is on, over the accounts.
    ///
    /// Each page is a list of its own, which is what remembers where a reader was in it: a
    /// page shown again is the page they left rather than one starting over.
    pub(super) fn open_add(&mut self, cx: &mut Context<Self>) {
        let add = self.add.clone();
        self.stack
            .update(cx, |stack, cx| stack.push(add, NavMotion::Animated, cx));
    }

    /// Pops the page that is on top, which is what the control in its header is for.
    ///
    /// The accounts are the root of the stack, so the page over them is the only thing there is
    /// to pop, and a stack left holding nothing is left alone rather than emptied.
    pub(super) fn go_back(&mut self, cx: &mut Context<Self>) {
        self.stack
            .update(cx, |stack, cx| stack.pop(NavMotion::Animated, cx));
    }

    /// Reads the accounts again.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.widget.upgrade() else {
            return;
        };
        pane.update(cx, |pane, cx| {
            pane.load_selections(cx);
        });
    }

    /// Searches X for the account the query names.
    ///
    /// The pane sends it — the binding is the pane's — while the query is this page's and so is
    /// the answer; whether a search is out is read from the pane, which is the only thing that
    /// can say what its binding is carrying. The control that sends one is disabled for a query
    /// that could not be answered, but the field also answers Enter, which reaches here without
    /// a control to have been disabled: the pane refuses that one the way it refuses a change.
    fn search(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.widget.upgrade() else {
            return;
        };
        let Some(query) = self.typed_query.clone() else {
            return;
        };
        let query = SmolStr::new(query.trim());
        if !valid_search_query(&query) {
            return;
        }
        pane.update(cx, |pane, cx| {
            pane.search_users(query, cx);
        });
    }

    /// Takes what a search answered, for the query it was asked with.
    ///
    /// An answer belongs to the query that asked for it: a reader who edited the query while
    /// the search was out asked about something else, and an answer to a question they no
    /// longer have is not one this page may draw. So the answer is dropped rather than shown
    /// under a query it does not stand for — and a search the dialog stopped waiting for, for
    /// the same reason, is dropped with it.
    pub(super) fn answered_search(
        &mut self,
        query: &SmolStr,
        answer: SearchAnswer,
        cx: &mut Context<Self>,
    ) {
        if self.typed_query.as_deref().map(str::trim) != Some(query.as_str()) {
            return;
        }
        self.search = Some(answer);
        cx.notify();
    }

    /// Monitors one account a search found.
    fn add(&mut self, username: SmolStr, cx: &mut Context<Self>) {
        let Some(pane) = self.widget.upgrade() else {
            return;
        };
        pane.update(cx, |pane, cx| {
            pane.add_selection(username, cx);
        });
    }

    /// Stops monitoring one account.
    fn delete(&mut self, x_user_id: SmolStr, cx: &mut Context<Self>) {
        let Some(pane) = self.widget.upgrade() else {
            return;
        };
        pane.update(cx, |pane, cx| {
            pane.delete_selection(x_user_id, cx);
        });
    }

    /// What the accounts are, and the controls one page offers over them.
    ///
    /// Every page carries a header of its own, so that going to the search and coming back is
    /// one page sliding over another rather than a title that stays put while what is under it
    /// changes. The page that searches X is a page pushed over the accounts, so the control
    /// that pops it is the one at the top of its own header, where a reader looks for it.
    fn header(
        &self,
        page: DialogPage,
        state: &DialogState,
        dialog: &Entity<Self>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let count = state.selections.as_ref().map_or_else(
            || t!(cx, "widget.x_account_monitoring.dialog_title"),
            |data| {
                t!(
                    cx,
                    "widget.x_account_monitoring.monitored_accounts",
                    current = data.selections().len(),
                    limit = data.limit()
                )
            },
        );
        let mut row = div()
            .debug_selector(|| "TEST_TWEET_HEADER".into())
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap_2();
        if page == DialogPage::Add {
            row = row.child(
                Button::new("tweet-monitor-back")
                    .ghost()
                    .medium()
                    .debug_selector(|| "TEST_TWEET_BACK".into())
                    .aria_label(t!(cx, "widget.x_account_monitoring.back"))
                    .with_tooltip(t!(cx, "widget.x_account_monitoring.back"))
                    .on_click({
                        let dialog = dialog.clone();
                        move |_, _, cx| {
                            dialog.update(cx, |dialog, cx| dialog.go_back(cx));
                        }
                    })
                    .child(Icon::new(IconKind::CaretLeft).size_3()),
            );
        }
        row = row.child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .font_weight(FontWeight::SEMIBOLD)
                .child(count),
        );
        // The controls the page that manages the accounts offers, in the order a reader reaches
        // for them: the one that adds an account — the page's own reason for being there — comes
        // before the one that only reads the same accounts again. The page that searches X
        // offers neither: it is a page over this one, and what it adds is added to this list.
        if page == DialogPage::Monitored {
            // A page with no accounts has nothing to add to, so the way to search for one
            // is disabled rather than opening a page that cannot add what it finds.
            row = row
                .child(
                    div().flex_none().child(
                        Button::new("tweet-monitor-add-account")
                            .filled()
                            .medium()
                            .debug_selector(|| "TEST_TWEET_ADD_ACCOUNT".into())
                            .disabled(state.selections.is_none())
                            .aria_label(t!(cx, "widget.x_account_monitoring.add_account"))
                            .with_tooltip(t!(cx, "widget.x_account_monitoring.add_account"))
                            .on_click({
                                let dialog = dialog.clone();
                                move |_, _, cx| {
                                    dialog.update(cx, |dialog, cx| dialog.open_add(cx));
                                }
                            })
                            .child(Icon::new(IconKind::Plus).size_3())
                            .child(t!(cx, "widget.x_account_monitoring.add_account")),
                    ),
                )
                .child(
                    div().flex_none().child(
                        Button::new("tweet-monitor-refresh")
                            .tonal()
                            .medium()
                            .debug_selector(|| "TEST_TWEET_REFRESH".into())
                            .disabled(
                                state.operation.is_some()
                                    || state.searching.is_some()
                                    || !state.ready,
                            )
                            .aria_label(t!(cx, "widget.x_account_monitoring.refresh"))
                            .with_tooltip(t!(cx, "widget.x_account_monitoring.refresh"))
                            .on_click({
                                let dialog = dialog.clone();
                                move |_, _, cx| {
                                    dialog.update(cx, |dialog, cx| dialog.reload(cx));
                                }
                            })
                            .child(Icon::new(IconKind::ArrowsClockwise).size_3()),
                    ),
                );
        }

        row.into_any_element()
    }

    /// The field one page is searched with: the accounts on screen, or X itself, and — on the
    /// page that searches X — the control that sends the search, beside the field rather than
    /// inside it.
    fn toolbar(
        &self,
        page: DialogPage,
        state: &DialogState,
        dialog: &Entity<Self>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let (field, selector, control) = match page {
            DialogPage::Monitored => (
                Input::new(&self.filter)
                    .medium()
                    .clearable(true)
                    .prefix_icon(IconKind::MagnifyingGlass)
                    .into_any_element(),
                "TEST_TWEET_FILTER_FIELD",
                None,
            ),
            DialogPage::Add => {
                // A search is sent over the binding a change is sent over, one call at a time,
                // so a search that is out is a search this control does not send again, and one
                // that cannot be sent while the accounts are being read or changed.
                let busy = state.operation.is_some() || state.searching.is_some() || !state.ready;
                let search = Button::new("tweet-monitor-search")
                    .ghost()
                    .medium()
                    .debug_selector(|| "TEST_TWEET_SEARCH".into())
                    .disabled(busy || !self.typed_query.as_deref().is_some_and(valid_search_query))
                    .aria_label(t!(cx, "widget.x_account_monitoring.search_users"))
                    .with_tooltip(t!(cx, "widget.x_account_monitoring.search_users"))
                    .on_click({
                        let dialog = dialog.clone();
                        move |_, _, cx| {
                            dialog.update(cx, |dialog, cx| dialog.search(cx));
                        }
                    })
                    .child(Icon::new(IconKind::MagnifyingGlass).size_3())
                    .into_any_element();
                (
                    Input::new(&self.query)
                        .medium()
                        .clearable(true)
                        .prefix_icon(IconKind::At)
                        .into_any_element(),
                    "TEST_TWEET_QUERY_FIELD",
                    Some(search),
                )
            }
        };

        div()
            .debug_selector(|| "TEST_TWEET_TOOLBAR".into())
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap_2()
            // The field is named by the page it is on, which is what a test measures the box it
            // is drawn in by: both pages draw one, and only one of them is on top.
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .debug_selector(move || selector.into())
                    .child(field),
            )
            .children(control.map(|control| div().flex_none().child(control)))
            .into_any_element()
    }

    /// Reports what the last change said, in an alert over the dialog.
    ///
    /// A report is read from the pane as the dialog is drawn, which is where a window to raise
    /// the alert on is. What the pane holds is reported once: an alert is opened at the end of
    /// the frame that opens it and asks for a frame of its own, so a dialog that reported the
    /// same thing on the frame after that would be drawing frames for an alert rather than for
    /// the accounts it is there for. What clears the record is the pane holding nothing, which
    /// is what makes the next change reported whether or not it says what the last one did.
    ///
    /// What the pane holds when the dialog opens was said before it opened, so it is already
    /// reported as far as this dialog is concerned: a notice is what a change says while the
    /// dialog that made it is there to hear it.
    fn report(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.widget.upgrade() else {
            return;
        };
        let notice = pane.read(cx).dialog_state().notice;
        if self.reported == notice {
            return;
        }
        self.reported = notice.clone();
        let Some(notice) = notice else {
            return;
        };

        let (title, message) = report_text(&notice, cx);
        let confirm = t!(cx, "common.confirm");
        // The builder runs once, when the alert's dialog mounts, so what it captures is moved into
        // the alert it builds rather than cloned for a builder that could be called again.
        window.alert(cx, move |alert, _, _| {
            alert.ok(title, message).ok_text(confirm)
        });
    }

    /// What one page draws above its list: its header, and the field that page is searched
    /// with.
    ///
    /// Both are read from the pane on every frame the page is drawn in, which is what keeps
    /// what a page offers as true as the state behind it.
    fn over(
        &mut self,
        page: DialogPage,
        pane: &Entity<XAccountMonitoringWidget>,
        dialog: &Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = pane.read(cx).dialog_state();
        let toolbar = match (state.signed_in, page) {
            (true, page) => Some(self.toolbar(page, &state, dialog, cx)),
            // Nothing is signed in, so there is nothing either page's field could be asked
            // about: the body says so, and a field over it would be a field that answers
            // nothing.
            (false, _) => None,
        };

        div()
            .w_full()
            .min_w_0()
            .flex_none()
            .flex()
            .flex_col()
            .gap_2()
            .child(self.header(page, &state, dialog, cx))
            .children(toolbar)
            .into_any_element()
    }

    /// The list one page draws, or why there is nothing to draw on it.
    ///
    /// A sentence stands in for the list by drawing the document the frame read into
    /// [`TweetDialog::resolve_message`], so nothing here parses anything: the three situations
    /// that have nothing to list are one document, read before this page is drawn.
    fn list(
        &mut self,
        page: DialogPage,
        pane: &Entity<XAccountMonitoringWidget>,
        dialog: &Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = pane.read(cx).dialog_state();
        if !state.signed_in {
            return centered_document(&self.message, cx.theme().roles.fg.muted.hsla);
        }
        match page {
            DialogPage::Monitored => self.monitored_body(&state, dialog, cx),
            DialogPage::Add => self.add_body(&state, cx),
        }
    }

    /// The accounts being monitored.
    fn monitored_body(
        &mut self,
        state: &DialogState,
        dialog: &Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let Some(data) = state.selections.as_ref() else {
            return match state.operation {
                // A read is on its way, whether it is out or waiting for the stream.
                Some(operation) if operation.is_read() => loading_message(
                    t!(cx, "widget.x_account_monitoring.loading_accounts"),
                    "TEST_TWEET_LOADING_ACCOUNTS",
                    cx,
                ),
                // Nothing is holding the account, so a read has nothing to answer it: the way
                // out is the recovery the pane itself offers, which is the same control the
                // pane is showing behind this dialog.
                _ if !state.ready => message_with_action(
                    t!(cx, "widget.x_account_monitoring.stream_unavailable"),
                    recovery_button(&state.recovery, cx),
                ),
                // The read was answered with a failure, which the alert over the dialog names:
                // what is left to draw here is the way to read the accounts again.
                _ => message_with_action(
                    t!(cx, "widget.x_account_monitoring.load_failed"),
                    self.retry_button(dialog, cx),
                ),
            };
        };
        if data.selections().is_empty() {
            return centered_document(&self.message, theme.roles.fg.muted.hsla);
        }

        let filtered = filtered_selection_indices(
            data.selections(),
            &state.hints,
            self.typed_filter.as_deref(),
        );
        if filtered.is_empty() {
            return centered_document(&self.message, theme.roles.fg.muted.hsla);
        }

        self.show_accounts(data.shared_selections(), &filtered, cx);

        div()
            .debug_selector(|| "TEST_TWEET_ACCOUNT_LIST".into())
            .size_full()
            .min_h_0()
            .child(self.accounts.clone())
            .into_any_element()
    }

    /// The accounts one search of X found, or what became of the search instead.
    ///
    /// A search is this page's own procedure, so everything it has to say is drawn here: the
    /// spinner while it is out, the words of the failure where it was asked for, and the answer
    /// itself. None of it is an alert over the dialog, which is for what a change to the
    /// accounts said.
    fn add_body(&mut self, state: &DialogState, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        if state.searching.is_some() {
            return loading_message(
                t!(cx, "widget.x_account_monitoring.searching_users"),
                "TEST_TWEET_SEARCHING",
                cx,
            );
        }
        let profiles = match self.search.as_ref() {
            // Nothing has been searched for — or the query was edited since — which is not an
            // empty answer: the field is what says what to do.
            None => return div().size_full().into_any_element(),
            // A search that could not be answered is drawn in the words the failure itself
            // gives, which is the same sentence every other failure of its kind is read in.
            Some(SearchAnswer::Failed(error)) => {
                return div()
                    .debug_selector(|| "TEST_TWEET_SEARCH_FAILED".into())
                    .size_full()
                    .child(centered_message(
                        error.localized_message(cx),
                        theme.roles.status.danger.base.hsla,
                    ))
                    .into_any_element();
            }
            Some(SearchAnswer::Found(profiles)) => profiles.clone(),
        };
        if profiles.is_empty() {
            return div()
                .debug_selector(|| "TEST_TWEET_NO_SEARCH_RESULTS".into())
                .size_full()
                .child(centered_message(
                    t!(cx, "widget.x_account_monitoring.no_search_results"),
                    theme.roles.fg.muted.hsla,
                ))
                .into_any_element();
        }

        self.show_results(&profiles, cx);

        div()
            .debug_selector(|| "TEST_TWEET_SEARCH_LIST".into())
            .size_full()
            .min_h_0()
            .child(self.results.clone())
            .into_any_element()
    }

    /// Shows the accounts the monitored page is drawing, if they are not the ones the list
    /// holds.
    ///
    /// A list draws what it was given and remembers where the reader is, so it is told about
    /// another set of accounts rather than rebuilt every frame: being told is what puts the
    /// reader back at the top, which is where another filter leaves them. What it was told is
    /// the pane's own rows and which of them were taken, so a frame that would give it the same
    /// rows again leaves the list — and the reader — alone.
    fn show_accounts(
        &mut self,
        selections: Arc<[XAccountMonitoringSelection]>,
        filtered: &[usize],
        cx: &mut Context<Self>,
    ) {
        let unchanged = self
            .shown_accounts
            .as_ref()
            .is_some_and(|(shown, indices)| Arc::ptr_eq(shown, &selections) && indices == filtered);
        if unchanged {
            return;
        }

        let accounts: Vec<XAccountMonitoringSelection> = filtered
            .iter()
            .filter_map(|index| selections.get(*index).cloned())
            .collect();
        self.shown_accounts = Some((selections, filtered.to_vec()));
        self.accounts.update(cx, |list, cx| {
            list.replace(accounts);
            cx.notify();
        });
    }

    /// Shows what a search found, if it is not what the list holds.
    fn show_results(&mut self, results: &Arc<[XAccountMonitoringProfile]>, cx: &mut Context<Self>) {
        if self
            .shown_results
            .as_ref()
            .is_some_and(|shown| Arc::ptr_eq(shown, results))
        {
            return;
        }

        self.shown_results = Some(results.clone());
        self.results.update(cx, |list, cx| {
            list.replace(results.iter().cloned());
            cx.notify();
        });
    }

    /// The way to read the accounts again, for a read that was answered with a failure.
    fn retry_button(&self, dialog: &Entity<Self>, cx: &Context<Self>) -> AnyElement {
        Button::new("tweet-monitor-retry")
            .outline()
            .small()
            .aria_label(t!(cx, "common.retry"))
            .on_click({
                let dialog = dialog.clone();
                move |_, _, cx| {
                    dialog.update(cx, |dialog, cx| dialog.reload(cx));
                }
            })
            .child(t!(cx, "common.retry"))
            .into_any_element()
    }

    /// The way out of the dialog, which belongs to both pages and so is drawn once under them
    /// rather than carried by either.
    ///
    /// The control closes the dialog by emitting, which is the dialog saying it is done to the
    /// layer that hosts it: it needs no window to hand and no way of being the dialog on top, and
    /// it is the signal the layer is listening for. A dismissal taken from the window instead would
    /// be a call to close *the dialog on top*, which this one may not be.
    fn footer(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .w_full()
            .flex_none()
            .flex()
            .flex_col()
            .gap_2()
            .child(Separator::horizontal())
            .child(
                div().w_full().flex().items_center().justify_end().child(
                    Button::new("tweet-monitor-close")
                        .ghost()
                        .medium()
                        .debug_selector(|| "TEST_TWEET_CLOSE".into())
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
                        .child(t!(cx, "common.close")),
                ),
            )
            .into_any_element()
    }
}

/// The dialog itself: the stack its two pages are drawn in, and the way out of it.
///
/// What a page draws is the page's own — the stack draws nothing but the page on top of it, or
/// the two of them while one slides over the other — so the dialog is left holding what both
/// pages are drawn against rather than what is drawn.
impl Render for TweetDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.report(window, cx);

        // The colour a page covers the one under it with is the panel's own surface, read once: a
        // page is the whole of what is drawn in the stack, so it is the surface the dialog is drawn
        // on that a page carries with it.
        let page_surface = cx.theme().components.dialog.background.hsla;

        div()
            .w(px(DIALOG_WIDTH))
            .h(px(DIALOG_HEIGHT))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                // One page over another: the page arriving slides in from the side a reader is
                // going to, and the one leaving drifts the other way rather than sliding out
                // from under it, so which page is on top reads from the direction of the move.
                NavStack::new(&self.stack)
                    // The stack takes what is left of the panel once the way out of it is laid
                    // out, which is what a column gives the control it does not size itself.
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .transition(Transition::new(PAGE_TRANSITION))
                    .item(move |page: NavPage, _, _| {
                        let offset = match (page.phase(), page.operation()) {
                            (
                                PresencePhase::Entering,
                                Some(NavOperation::Push | NavOperation::Replace),
                            ) => 1.0 - page.progress(),
                            (PresencePhase::Exiting, Some(NavOperation::Pop)) => page.progress(),
                            (PresencePhase::Exiting, Some(NavOperation::Push)) => {
                                -PAGE_DEPTH_DRIFT * page.progress()
                            }
                            (PresencePhase::Entering, Some(NavOperation::Pop)) => {
                                -PAGE_DEPTH_DRIFT * (1.0 - page.progress())
                            }
                            _ => 0.0,
                        };
                        // A page is the width of the panel wherever it is in its journey: an
                        // absolute box whose width is left to its two edges is the space between
                        // them, so a page placed off the side would be squeezed as it travelled
                        // rather than carried across. And it carries the panel's surface with it,
                        // because two pages are drawn at once: without it, what is under the page
                        // on top would read through the header and the field of the one over it.
                        page.w_full()
                            .left(relative(offset))
                            .bg(page_surface)
                            .into_any_element()
                    }),
            )
            .child(self.footer(cx))
            .into_any_element()
    }
}

/// What one report says: the title of the alert it is raised in, and what that alert is about.
///
/// Which way the change went is the title, and what the service answered is the message: a
/// failure is read in the words the failure itself gives, rather than in a sentence written
/// around it here.
fn report_text(notice: &SelectionNotice, cx: &App) -> (SharedString, SharedString) {
    match notice {
        SelectionNotice::Added => (
            t!(cx, "widget.x_account_monitoring.success_title"),
            t!(cx, "widget.x_account_monitoring.add_success"),
        ),
        SelectionNotice::Problem(failure) => (
            t!(cx, "widget.x_account_monitoring.error_title"),
            failure.localized_message(cx),
        ),
    }
}

/// A sentence standing in the middle of a page, drawn from the document it was read into.
///
/// The copy is catalogue copy, and the sentences of the catalogue are written as markdown — the
/// ones this dialog shares with the pane's own placeholder carry a `<p align="center">` tag pair
/// — so it is drawn by the markdown component rather than as the string it is written in. The
/// document is the dialog's own, which is what keeps the parse out of the frame that draws it.
fn centered_document(message: &Entity<MarkdownState>, color: Hsla) -> AnyElement {
    div()
        .size_full()
        .min_h_0()
        .flex()
        .items_center()
        .justify_center()
        .px_4()
        .text_center()
        .text_sm()
        .text_color(color)
        .child(Markdown::new(message))
        .into_any_element()
}

/// One of the dialog's two pages: everything the dialog draws above its list, and the list.
///
/// A page is a view rather than a part of a frame, because it is the stack that says which one
/// is showing and for how long both of them are drawn. What a page reads — the pane, and the
/// dialog that owns what the user is typing — it reads through weak handles, so a page never
/// keeps either of them alive.
struct PageView {
    widget: WeakEntity<XAccountMonitoringWidget>,
    dialog: WeakEntity<TweetDialog>,
    page: DialogPage,
}

impl PageView {
    fn new(
        widget: WeakEntity<XAccountMonitoringWidget>,
        dialog: WeakEntity<TweetDialog>,
        page: DialogPage,
    ) -> Self {
        Self {
            widget,
            dialog,
            page,
        }
    }
}

impl Render for PageView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (Some(pane), Some(dialog)) = (self.widget.upgrade(), self.dialog.upgrade()) else {
            return div().into_any_element();
        };
        let page = self.page;
        let (over, list) = dialog.update(cx, |dialog, cx| {
            // The sentence a page's list stands in with is read into the dialog's own document
            // before that page is drawn, so the frame that shows a different one is the frame
            // that parsed it.
            dialog.resolve_message(&pane, cx);
            let handle = cx.entity();
            (
                dialog.over(page, &pane, &handle, cx),
                dialog.list(page, &pane, &handle, cx),
            )
        });
        let theme = cx.theme();

        div()
            .debug_selector(move || match page {
                DialogPage::Monitored => "TEST_TWEET_MONITORED_PAGE".into(),
                DialogPage::Add => "TEST_TWEET_ADD_PAGE".into(),
            })
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_2()
            .child(over)
            .child(
                div()
                    .min_w_0()
                    .min_h_0()
                    .flex_1()
                    .overflow_hidden()
                    .rounded_sm()
                    .border_1()
                    .border_color(theme.roles.line.divider.hsla)
                    .bg(theme.roles.surface.raised.hsla)
                    .child(list),
            )
            .into_any_element()
    }
}

/// The pane's own state, read for the frame that draws it and again for each row the lists
/// draw, which is why what a row reads is shared rather than copied.
pub(super) struct DialogState {
    signed_in: bool,
    ready: bool,
    selections: Option<SelectionData>,
    /// The query the pane's binding is carrying a search for, while it is carrying one.
    ///
    /// A search is the page's own, and so is what it found; what the page cannot know is
    /// whether the call is still the pane's to be answered, which is what this says. It is read
    /// for the spinner the page draws and for the controls a change is asked with: one call at
    /// a time is what the binding carries, so a search that is out is a change that cannot be.
    searching: Option<SmolStr>,
    operation: Option<SelectionOperation>,
    notice: Option<SelectionNotice>,
    hints: Arc<HashMap<SmolStr, SharedString>>,
    media: MediaStates,
    recovery: WidgetAction,
}

impl DialogState {
    /// The state one pane is in, as the pane holds it.
    pub(super) fn new(
        signed_in: bool,
        ready: bool,
        selections: Option<SelectionData>,
        searching: Option<SmolStr>,
        operation: Option<SelectionOperation>,
        notice: Option<SelectionNotice>,
        hints: Arc<HashMap<SmolStr, SharedString>>,
        media: MediaStates,
        recovery: WidgetAction,
    ) -> Self {
        Self {
            signed_in,
            ready,
            selections,
            searching,
            operation,
            notice,
            hints,
            media,
            recovery,
        }
    }

    /// Whether a change may be asked for right now.
    pub(super) fn may_change(&self) -> bool {
        self.ready
            && self.selections.is_some()
            && self.operation.is_none()
            && self.searching.is_none()
    }
}

/// What one search-result row needs to decide what its control does.
struct SearchRows<'a> {
    /// The accounts being monitored, which is what says whether this one already is.
    monitored: Option<&'a SelectionData>,
    /// The names those accounts are known by, which is what they are asked about by name.
    hints: &'a HashMap<SmolStr, SharedString>,
    quota_full: bool,
    enabled: bool,
}

impl<'a> SearchRows<'a> {
    fn of(state: &'a DialogState) -> Self {
        Self {
            monitored: state.selections.as_ref(),
            hints: &state.hints,
            quota_full: state
                .selections
                .as_ref()
                .is_some_and(SelectionData::quota_full),
            enabled: state.may_change(),
        }
    }

    /// What the control beside one search result does.
    ///
    /// Whether an account is already monitored is the question the rows of the accounts list
    /// answer, asked of the same set by the same rule: there is no second list of names here
    /// that could say something the list itself would not.
    fn action(&self, username: &str) -> SearchProfileAction {
        let monitored = self
            .monitored
            .is_some_and(|data| data.contains_username(username, self.hints));
        SearchProfileAction::new(monitored, !self.enabled || self.quota_full)
    }
}

/// What the control beside one search result is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchProfileAction {
    /// The account may be monitored.
    Add { disabled: bool },
    /// The account is already monitored.
    Selected,
}

impl SearchProfileAction {
    const fn new(selected: bool, disabled: bool) -> Self {
        if selected {
            Self::Selected
        } else {
            Self::Add { disabled }
        }
    }

    const fn disabled(self) -> bool {
        match self {
            Self::Add { disabled } => disabled,
            Self::Selected => true,
        }
    }

    const fn icon(self) -> IconKind {
        match self {
            Self::Add { .. } => IconKind::Plus,
            Self::Selected => IconKind::Check,
        }
    }
}

/// Which of the monitored accounts a filter leaves, in the order they are held.
///
/// The filter is a convenience over what is already on screen: an account is matched by its
/// username, its display name, the username it is waiting to be activated under, or any part
/// of its numeric id.
fn filtered_selection_indices(
    selections: &[XAccountMonitoringSelection],
    hints: &HashMap<SmolStr, SharedString>,
    filter: Option<&str>,
) -> Vec<usize> {
    let Some(filter) = filter.map(str::trim).filter(|filter| !filter.is_empty()) else {
        return (0..selections.len()).collect();
    };
    let filter = filter.to_lowercase();
    let filter = filter.strip_prefix('@').unwrap_or(&filter);

    selections
        .iter()
        .enumerate()
        .filter(|(_, selection)| {
            let mut fields = vec![selection.x_user_id.as_str()];
            if let Some(profile) = selection.profile.as_ref() {
                fields.push(profile.username.as_str());
                fields.push(profile.display_name.as_str());
            }
            // A row waiting to be activated is matched by the name it is drawn with, which is
            // the only one it has.
            if let Some(name) = selection_name(selection, hints) {
                fields.push(name);
            }
            fields
                .into_iter()
                .any(|field| field.to_lowercase().contains(filter))
        })
        .map(|(index, _)| index)
        .collect()
}

/// One monitored account: which account it is, and what the account is doing with it.
///
/// The pane is read for what the row draws rather than handed the answer, so that a row drawn
/// by the list reads what the pane holds now: a list draws its rows again on every frame of the
/// dialog, and what a row draws is the frame it is drawn in.
fn account_row(
    selection: &XAccountMonitoringSelection,
    pane: &WeakEntity<XAccountMonitoringWidget>,
    dialog: &WeakEntity<TweetDialog>,
    cx: &App,
) -> AnyElement {
    let Some(pane) = pane.upgrade() else {
        return div().into_any_element();
    };
    let state = pane.read(cx).dialog_state();
    let theme = cx.theme();
    let x_user_id = selection.x_user_id.clone();
    let name = account_name(selection, &state.hints);
    let label = t!(
        cx,
        "widget.x_account_monitoring.delete_account",
        account = name
    );
    let delete = Button::new((
        ElementId::Name(SharedString::new_static("TweetMonitorAccountDelete")),
        selection.x_user_id.as_str(),
    ))
    .ghost()
    .small()
    .disabled(!state.may_change())
    .aria_label(label.clone())
    .with_tooltip(label)
    .on_click({
        let dialog = dialog.clone();
        move |_, _, cx| {
            let x_user_id = x_user_id.clone();
            let Some(dialog) = dialog.upgrade() else {
                return;
            };
            dialog.update(cx, |dialog, cx| dialog.delete(x_user_id, cx));
        }
    })
    .child(Icon::new(IconKind::Trash).size_3());

    let mut column = div().min_w_0().flex_1().flex().flex_col().gap_1();
    match selection.profile.as_ref() {
        // The profile line already names the account, so the name is not repeated above it.
        Some(profile) => {
            column = column.child(profile_line(profile, cx)).child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(theme.roles.fg.muted.hsla)
                    .child(SharedString::new(profile.description.as_str())),
            );
        }
        // An account that is waiting to be activated has no profile yet, so what is known
        // about it is its name, its state and its id.
        None => {
            column = column
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.roles.fg.default.hsla)
                        .child(name),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(theme.roles.fg.muted.hsla)
                        .child(status_label(selection.status, cx)),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(theme.roles.fg.muted.hsla)
                        .child(t!(
                            cx,
                            "widget.x_account_monitoring.account_id",
                            id = selection.x_user_id.clone()
                        )),
                );
        }
    }

    Interactive::new((
        ElementId::Name(SharedString::new_static("TweetMonitorAccountRow")),
        selection.x_user_id.as_str(),
    ))
    .rounded_sm()
    .ghost()
    .medium()
    // The row is given the height every place of its list is laid out at rather than the height
    // of a control: a control is one line tall, and this row is a face with the lines beside it,
    // so a row sized as a control is a row its own content overflows.
    .h(px(ROW_HEIGHT))
    .debug_selector(|| ACCOUNT_ROW_SELECTOR.into())
    .child(
        div()
            .min_w_0()
            .flex_1()
            .flex()
            .items_center()
            .gap_3()
            .child(avatar(
                selection
                    .profile
                    .as_ref()
                    .map_or("", |profile| profile.avatar_url.as_str()),
                &state.media,
                ElementId::Name(SharedString::new(format_smolstr!(
                    "tweet-account-avatar-{}",
                    selection.x_user_id
                ))),
                Some(ACCOUNT_AVATAR_SELECTOR),
                theme.roles.surface.raised.hsla,
                theme.roles.fg.muted.hsla,
                px(ROW_AVATAR_SIZE),
            ))
            .child(column)
            .child(
                Badge::new()
                    .small()
                    .child(status_label(selection.status, cx)),
            ),
    )
    .child(delete)
    .into_any_element()
}

/// One account a search found: who they are, and the control that monitors them.
///
/// What the control does is read from the pane as the row is drawn, so that an account the
/// service confirmed is one the row beside it stops offering: the list draws its rows again on
/// every frame of the dialog.
fn search_result_row(
    profile: &XAccountMonitoringProfile,
    pane: &WeakEntity<XAccountMonitoringWidget>,
    dialog: &WeakEntity<TweetDialog>,
    cx: &App,
) -> AnyElement {
    let Some(pane) = pane.upgrade() else {
        return div().into_any_element();
    };
    let state = pane.read(cx).dialog_state();
    let rows = SearchRows::of(&state);
    let theme = cx.theme();
    let action = rows.action(profile.username.as_str());
    let username = SmolStr::new(profile.username.as_str());
    let label = match action {
        SearchProfileAction::Selected => t!(cx, "widget.x_account_monitoring.already_monitored"),
        SearchProfileAction::Add { .. } => t!(
            cx,
            "widget.x_account_monitoring.add_search_result",
            username = profile.username.clone()
        ),
    };
    let control = Button::new((
        ElementId::Name(SharedString::new_static("TweetMonitorSearchAdd")),
        profile.x_user_id.as_str(),
    ))
    .ghost()
    .small()
    .disabled(action.disabled())
    .aria_label(label.clone())
    .with_tooltip(label)
    .on_click({
        let dialog = dialog.clone();
        move |_, _, cx| {
            let username = username.clone();
            let Some(dialog) = dialog.upgrade() else {
                return;
            };
            dialog.update(cx, |dialog, cx| dialog.add(username, cx));
        }
    })
    .child(Icon::new(action.icon()).size_3());

    Interactive::new((
        ElementId::Name(SharedString::new_static("TweetMonitorSearchRow")),
        profile.x_user_id.as_str(),
    ))
    .rounded_sm()
    .ghost()
    .medium()
    // The row is given the height every place of its list is laid out at, which is what a row
    // of a face with lines beside it needs to be: the height of a control is not.
    .h(px(ROW_HEIGHT))
    .debug_selector(|| SEARCH_ROW_SELECTOR.into())
    .child(
        div()
            .min_w_0()
            .flex_1()
            .flex()
            .items_center()
            .gap_3()
            .child(avatar(
                profile.avatar_url.as_str(),
                &state.media,
                ElementId::Name(SharedString::new(format_smolstr!(
                    "tweet-search-avatar-{}",
                    profile.x_user_id
                ))),
                Some(SEARCH_AVATAR_SELECTOR),
                theme.roles.surface.raised.hsla,
                theme.roles.fg.muted.hsla,
                px(ROW_AVATAR_SIZE),
            ))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(profile_line(profile, cx))
                    .when(!profile.description.is_empty(), |this| {
                        this.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(theme.roles.fg.muted.hsla)
                                .child(SharedString::new(profile.description.as_str())),
                        )
                    })
                    // Counts a search did not carry are both zero, and a line of zeros says
                    // nothing about the account.
                    .when(
                        profile.followers_count > 0 || profile.following_count > 0,
                        |this| {
                            this.child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(theme.roles.fg.muted.hsla)
                                    // A count of one is the singular word: "1 follower".
                                    .child(if profile.followers_count == 1 {
                                        t!(
                                            cx,
                                            "widget.x_account_monitoring.followers_following_one_follower",
                                            followers =
                                                format_follower_count(profile.followers_count),
                                            following = profile.following_count
                                        )
                                    } else {
                                        t!(
                                            cx,
                                            "widget.x_account_monitoring.followers_following",
                                            followers =
                                                format_follower_count(profile.followers_count),
                                            following = profile.following_count
                                        )
                                    }),
                            )
                        },
                    ),
            ),
    )
    .child(control)
    .into_any_element()
}

/// The name line of one account: what it is called, and whether X vouches for it.
fn profile_line(profile: &XAccountMonitoringProfile, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let mut line = div().min_w_0().flex().items_center().gap_2().child(
        div()
            .min_w_0()
            .truncate()
            .font_weight(FontWeight::MEDIUM)
            .debug_selector(|| "TEST_TWEET_PROFILE_NAME".into())
            .child(SharedString::new(profile.display_name.as_str())),
    );
    if profile.verified {
        line = line.child(
            div()
                .flex_none()
                .debug_selector(|| "TEST_TWEET_PROFILE_VERIFIED".into())
                .child(
                    Icon::new(IconKind::CheckCircle)
                        .size_3()
                        .text_color(theme.roles.brand.base.hsla),
                ),
        );
    }
    if profile.protected {
        line = line.child(
            div()
                .flex_none()
                .text_xs()
                .text_color(theme.roles.fg.muted.hsla)
                .child(t!(cx, "widget.x_account_monitoring.protected")),
        );
    }

    line.child(
        div()
            .min_w_0()
            .flex_1()
            .truncate()
            .text_xs()
            .text_color(theme.roles.fg.muted.hsla)
            .child(SharedString::new(format_smolstr!(
                "@{}",
                profile.username.as_str()
            ))),
    )
    .into_any_element()
}

/// What one monitored account is called, and what it is called while it waits.
///
/// A row is named by what is known about it — its profile, or the username it was added under
/// while it waits to be activated — which is the same rule that decides whether an account is
/// already monitored, asked in the same place.
fn account_name(
    selection: &XAccountMonitoringSelection,
    hints: &HashMap<SmolStr, SharedString>,
) -> SharedString {
    match selection_name(selection, hints) {
        Some(name) => SharedString::new(format_smolstr!("@{name}")),
        None => SharedString::new(selection.x_user_id.as_str()),
    }
}

/// How many followers an account has, at the width a row has for it.
///
/// A row is one line, so a count is read in the largest unit that keeps it under a thousand,
/// to one tenth: 1250 is "1.3K". A count whose tenth rounds up to the next unit reads as that
/// unit — 999_950 is "1M" rather than "1000.0K" — which is what makes a row's width depend on
/// the number rather than on the unit it happens to be near.
fn format_follower_count(count: u64) -> SmolStr {
    const UNITS: [(u64, &str); 3] = [(1_000, "K"), (1_000_000, "M"), (1_000_000_000, "B")];
    let mut formatted: Option<SmolStr> = None;
    for (index, (scale, suffix)) in UNITS.iter().enumerate() {
        if count < *scale {
            break;
        }
        let tenths = (count % scale) * 10 + scale / 2;
        if tenths >= scale * 10 {
            // The tenth carried into the unit above this one, and a larger unit that also
            // fits replaces whatever this turn would have written.
            formatted = match UNITS.get(index + 1) {
                Some((next_scale, next_suffix)) => Some(format_smolstr!(
                    "{}{next_suffix}",
                    (count + next_scale / 2) / next_scale
                )),
                None => Some(format_smolstr!("{}{suffix}", count / scale + 1)),
            };
            continue;
        }
        let whole = count / scale;
        let tenths = tenths / scale;
        formatted = Some(if tenths == 0 {
            format_smolstr!("{whole}{suffix}")
        } else {
            format_smolstr!("{whole}.{tenths}{suffix}")
        });
    }
    formatted.unwrap_or_else(|| format_smolstr!("{count}"))
}

/// A message in the middle of the panel, with the spinner that says it is being waited for.
///
/// `selector` names which of the panel's waits this is, because the two pages wait for
/// different things and a test that asserts one page is not waiting has to be able to say which.
fn loading_message(
    message: SharedString,
    selector: &'static str,
    cx: &Context<TweetDialog>,
) -> AnyElement {
    div()
        .id("tweet-monitor-loading")
        .debug_selector(move || selector.into())
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .role(Role::ProgressIndicator)
        .aria_label(message.clone())
        .child(div().size_5().child(Spinner::new("tweet-monitor-spinner")))
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().roles.fg.muted.hsla)
                .child(message),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;
    use i18n::language::codes::LanguageCode;
    use kytrix_service_provider::service::x::{
        XAccountMonitoringApiError, XAccountMonitoringProfile, XAccountMonitoringSelection,
        XAccountMonitoringSelectionStatus, XAccountMonitoringSelectionsResponse,
    };

    use super::*;
    use crate::view::SelectionFailure;

    /// The alert a change is reported in says which way it went, in either language.
    ///
    /// The title and the message are all an alert is made of, and a key a locale does not define
    /// translates to an empty string rather than failing: a typo in one of them would otherwise
    /// reach the user as an alert with a blank panel, which is what both are read for here.
    #[gpui_kit::test]
    fn a_report_should_be_written_in_both_languages(cx: &mut TestAppContext) {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales should load");
        });

        for (language, title, added_title) in [
            (LanguageCode::En, "Error", "Success"),
            (LanguageCode::ZhCn, "错误", "成功"),
        ] {
            cx.update(|cx| cx.set_global(language));

            let (problem, reason, success, joined) = cx.read(|cx| {
                let (problem, reason) = report_text(
                    &SelectionNotice::Problem(SelectionFailure::Rejected(
                        XAccountMonitoringApiError::QuotaExceeded,
                    )),
                    cx,
                );
                let (success, joined) = report_text(&SelectionNotice::Added, cx);

                (problem, reason, success, joined)
            });

            assert_eq!(
                problem.as_ref(),
                title,
                "a change that was refused should be titled in {language:?}"
            );
            assert_eq!(
                success.as_ref(),
                added_title,
                "an account that joined the list should be titled in {language:?}"
            );
            assert!(
                !reason.is_empty() && !reason.contains("%{"),
                "the reason should be written in {language:?}: {reason}"
            );
            assert!(
                !joined.is_empty() && !joined.contains("%{"),
                "the account that joined the list should be named in {language:?}: {joined}"
            );
        }
    }

    /// One monitored account, under an id and a name of the test's choosing.
    fn monitored(x_user_id: &str, username: Option<&str>) -> XAccountMonitoringSelection {
        XAccountMonitoringSelection {
            x_user_id: x_user_id.into(),
            status: XAccountMonitoringSelectionStatus::Active,
            created_at: 1,
            updated_at: 2,
            profile: username.map(|username| XAccountMonitoringProfile {
                x_user_id: x_user_id.into(),
                username: username.into(),
                display_name: SmolStr::new(format!("{username} display")),
                avatar_url: SmolStr::new(format!("https://pbs.twimg.com/{username}.jpg")),
                description: SmolStr::new_static(""),
                followers_count: 0,
                following_count: 0,
                protected: false,
                verified: false,
            }),
        }
    }

    #[test]
    fn the_filter_should_match_what_a_reader_can_see() {
        let selections = [
            monitored("42", Some("Kytrix")),
            monitored("43", None),
            monitored("9876543210", Some("someone")),
        ];
        let hints = HashMap::from([(
            SmolStr::new_static("43"),
            SharedString::new_static("pending"),
        )]);

        // Nothing typed is everything on screen, and a filter that matches nothing is nothing.
        assert_eq!(
            filtered_selection_indices(&selections, &hints, None),
            [0, 1, 2]
        );
        assert_eq!(
            filtered_selection_indices(&selections, &hints, Some("   ")),
            [0, 1, 2]
        );
        assert!(filtered_selection_indices(&selections, &hints, Some("nobody")).is_empty());

        // A username, a display name, a hint and a numeric id are all things a reader reads.
        assert_eq!(
            filtered_selection_indices(&selections, &hints, Some("kytrix")),
            [0]
        );
        assert_eq!(
            filtered_selection_indices(&selections, &hints, Some("Kytrix display")),
            [0]
        );
        assert_eq!(
            filtered_selection_indices(&selections, &hints, Some("@pending")),
            [1]
        );
        assert_eq!(
            filtered_selection_indices(&selections, &hints, Some("65432")),
            [2]
        );
    }

    #[test]
    fn a_follower_count_should_read_at_the_width_a_row_has() {
        for (count, expected) in [
            (0, "0"),
            (999, "999"),
            (1_000, "1K"),
            (1_250, "1.3K"),
            (1_000_000, "1M"),
            (1_250_000, "1.3M"),
            (1_000_000_000, "1B"),
            (999_950, "1M"),
            (999_999_950, "1B"),
        ] {
            assert_eq!(format_follower_count(count), expected, "{count} followers");
        }
    }

    /// Whether a search result is one already monitored is the question the monitored list
    /// answers, asked of the same rows by the same rule: the control beside a result never reads
    /// as monitored for a name the list itself would not show.
    #[test]
    fn a_search_result_should_offer_to_monitor_it_unless_it_already_is() {
        let hints = HashMap::from([
            (SmolStr::new_static("42"), SharedString::new_static("stale")),
            (
                SmolStr::new_static("43"),
                SharedString::new_static("pending"),
            ),
        ]);
        let data = SelectionData::new(XAccountMonitoringSelectionsResponse {
            selections: vec![
                monitored("42", Some("kytrix")),
                monitored("43", None),
                monitored("44", None),
            ],
            x_account_limit: 4,
        });
        let rows = SearchRows {
            monitored: Some(&data),
            hints: &hints,
            quota_full: false,
            enabled: true,
        };

        assert_eq!(
            rows.action("kytrix"),
            SearchProfileAction::Selected,
            "an account already monitored is not one to monitor again"
        );
        assert_eq!(
            rows.action("Kytrix"),
            SearchProfileAction::Selected,
            "a username is the same name whatever its case"
        );
        assert_eq!(
            rows.action("pending"),
            SearchProfileAction::Selected,
            "a row waiting to be activated is monitored under the name it was added with"
        );
        assert_eq!(
            rows.action("stale"),
            SearchProfileAction::Add { disabled: false },
            "a row a profile names is not also named by what is remembered for its id"
        );
        assert_eq!(
            rows.action("44"),
            SearchProfileAction::Add { disabled: false },
            "a row nothing names is not one a name can read as monitored"
        );
        assert_eq!(
            rows.action("someone"),
            SearchProfileAction::Add { disabled: false }
        );
        assert!(!rows.action("someone").disabled());

        let full = SearchRows {
            quota_full: true,
            ..rows
        };
        assert_eq!(
            full.action("someone"),
            SearchProfileAction::Add { disabled: true }
        );
        assert!(full.action("someone").disabled());

        let busy = SearchRows {
            enabled: false,
            ..full
        };
        assert_eq!(
            busy.action("someone"),
            SearchProfileAction::Add { disabled: true }
        );
    }

    #[test]
    fn a_monitored_account_should_be_named_by_what_is_known_about_it() {
        let hints = HashMap::from([(
            SmolStr::new_static("43"),
            SharedString::new_static("pending"),
        )]);

        assert_eq!(
            account_name(&monitored("42", Some("kytrix")), &hints),
            "@kytrix"
        );
        assert_eq!(
            account_name(&monitored("43", None), &hints),
            "@pending",
            "an account waiting to be activated is named by what was asked for"
        );
        assert_eq!(
            account_name(&monitored("44", None), &hints),
            "44",
            "nothing but the id is known, so the id is what a row says"
        );
    }
}
