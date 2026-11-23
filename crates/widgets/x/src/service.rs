use std::{collections::VecDeque, future::Future, path::PathBuf, sync::Arc};

use ahash::{AHashMap as HashMap, AHashSet as HashSet};
use i18n::translate::Translate;
use kytrix_service_provider::service::{
    connector::KytrixServiceConnector,
    engine::{KytrixAccountSession, KytrixServiceError, XSubscriptionEvent, XTweetBatch},
    protocol::{valid_x_id, valid_x_username},
    x::{MAX_X_SELECTIONS, XApiError, XMutationOutcome, XProfile, XSelection, XSelectionsResponse},
};
use network::transport::{
    error::TransportError,
    subscription::{Subscription, SubscriptionStatus},
};
use smol_str::SmolStr;
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use widget::{
    error::TranslatedError,
    protocol::WidgetProtocol,
    runtime::{ServiceContext, WidgetService},
    stream::{StreamPublishError, StreamPublisher},
};

use super::{
    media::{
        MEDIA_OPERATION_TIMEOUT, MediaDownloader, MediaError, MediaTransport, SharedTransport,
    },
    model::{MediaState, MediaStates, SelectionData, TweetSnapshot, TweetState},
};

const MEDIA_CONCURRENCY: usize = 4;

const MEDIA_RETRY_CAPACITY: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum TweetRequest {
    Load(TweetScope),
    Retry(TweetScope),
    Stop,
    FetchSelections(TweetScope),
    WatchSelections {
        scope: TweetScope,
        watched: bool,
    },
    SearchUsers {
        scope: TweetScope,
        query: SmolStr,
    },
    AddSelection {
        scope: TweetScope,
        username: SmolStr,
    },
    DeleteSelection {
        scope: TweetScope,
        x_user_id: SmolStr,
    },
    RetryMedia {
        scope: TweetScope,
        url: SmolStr,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum TweetResponse {
    StreamStarted,
    Stopped,
    Selections(SelectionData),
    SelectionsWatched,
    SearchResults(Arc<[XProfile]>),
    Added(XMutationOutcome<XSelection>),
    Deleted(XMutationOutcome<()>),
    MediaRetried,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct TweetScope {
    account_id: SmolStr,
}

impl TweetScope {
    pub(super) fn new(account_id: Option<&SmolStr>) -> Option<Self> {
        let account_id = account_id.filter(|account_id| !account_id.is_empty())?;
        Some(Self {
            account_id: account_id.clone(),
        })
    }

    pub(super) fn account_id(&self) -> &str {
        &self.account_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum TweetStatus {
    Connecting,
    Ready,
    Failed(TweetStreamError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum TweetStreamError {
    Service(KytrixServiceError),
    Api(XApiError),
    Transport(TransportError),
}

impl TranslatedError for TweetStreamError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Service(error) => error.translated_message(translate),
            Self::Api(error) => error.translated_message(translate),
            Self::Transport(error) => error.translated_message(translate),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum TweetStreamMessage {
    Status(TweetStatus),
    Update(TweetUpdate),
}

#[derive(Clone, Debug)]
pub(super) struct TweetUpdate {
    revision: u64,
    feed: TweetSnapshot,
    media: MediaStates,
}

impl TweetUpdate {
    pub(super) const fn revision(&self) -> u64 {
        self.revision
    }

    pub(super) fn feed(&self) -> &TweetSnapshot {
        &self.feed
    }

    pub(super) fn media(&self) -> &MediaStates {
        &self.media
    }

    #[cfg(test)]
    pub(super) const fn published(revision: u64, feed: TweetSnapshot, media: MediaStates) -> Self {
        Self {
            revision,
            feed,
            media,
        }
    }
}

pub(super) struct TweetProtocol;

impl WidgetProtocol for TweetProtocol {
    type Request = TweetRequest;
    type Response = TweetResponse;
    type StreamKey = TweetScope;
    type StreamMessage = TweetStreamMessage;
    type Error = XApiError;
}

pub(super) trait TweetSource: Clone + Send + Sync + 'static {
    type Session: TweetSession;

    fn acquire_session(
        &self,
        account_id: &str,
    ) -> impl Future<Output = Result<Self::Session, KytrixServiceError>> + Send;
}

pub(super) trait TweetSession: Clone + Send + Sync + 'static {
    type Subscription: TweetEventSubscription;

    fn account_id(&self) -> &str;

    fn subscribe_x(
        &self,
    ) -> impl Future<Output = Result<Self::Subscription, KytrixServiceError>> + Send;

    fn fetch_tweets(&self) -> impl Future<Output = Result<XTweetBatch, XApiError>> + Send;

    fn fetch_selections(
        &self,
    ) -> impl Future<Output = Result<XSelectionsResponse, XApiError>> + Send;

    fn search_users(
        &self,
        query: &str,
    ) -> impl Future<Output = Result<Vec<XProfile>, XApiError>> + Send;

    fn add_selection(
        &self,
        username: &str,
    ) -> impl Future<Output = XMutationOutcome<XSelection>> + Send;

    fn delete_selection(
        &self,
        x_user_id: &str,
    ) -> impl Future<Output = XMutationOutcome<()>> + Send;
}

pub(super) trait TweetEventSubscription: Send + 'static {
    fn observe_status(&self) -> watch::Receiver<SubscriptionStatus>;

    fn recv(&mut self) -> impl Future<Output = Result<XSubscriptionEvent, TransportError>> + Send;
}

impl TweetSource for KytrixServiceConnector {
    type Session = KytrixAccountSession;

    async fn acquire_session(&self, account_id: &str) -> Result<Self::Session, KytrixServiceError> {
        self.acquire_account_session(account_id).await
    }
}

impl TweetSession for KytrixAccountSession {
    type Subscription = Subscription<XSubscriptionEvent>;

    fn account_id(&self) -> &str {
        self.profile().account_id()
    }

    async fn subscribe_x(&self) -> Result<Self::Subscription, KytrixServiceError> {
        KytrixAccountSession::subscribe_x(self).await
    }

    async fn fetch_tweets(&self) -> Result<XTweetBatch, XApiError> {
        self.fetch_x_tweets().await
    }

    async fn fetch_selections(&self) -> Result<XSelectionsResponse, XApiError> {
        self.fetch_x_selections().await
    }

    async fn search_users(&self, query: &str) -> Result<Vec<XProfile>, XApiError> {
        self.search_x_users(query).await
    }

    async fn add_selection(&self, username: &str) -> XMutationOutcome<XSelection> {
        KytrixAccountSession::add_x_selection(self, username).await
    }

    async fn delete_selection(&self, x_user_id: &str) -> XMutationOutcome<()> {
        KytrixAccountSession::delete_x_selection(self, x_user_id).await
    }
}

impl TweetEventSubscription for Subscription<XSubscriptionEvent> {
    fn observe_status(&self) -> watch::Receiver<SubscriptionStatus> {
        Subscription::observe_status(self)
    }

    async fn recv(&mut self) -> Result<XSubscriptionEvent, TransportError> {
        Subscription::recv(self).await
    }
}

pub(super) struct TweetService<S = KytrixServiceConnector, T = SharedTransport> {
    source: S,
    downloader: MediaDownloader<T>,
}

impl<S, T> TweetService<S, T> {
    pub(super) const fn new(source: S, downloader: MediaDownloader<T>) -> Self {
        Self { source, downloader }
    }
}

struct ActiveSession<S> {
    scope: TweetScope,
    session: S,
    auxiliary: watch::Sender<Arc<[SmolStr]>>,
    retries: mpsc::Sender<SmolStr>,
}

impl<S> ActiveSession<S> {
    fn session(&self) -> &S {
        &self.session
    }
}

impl<S, T> WidgetService<TweetProtocol> for TweetService<S, T>
where
    S: TweetSource,
    T: MediaTransport,
{
    async fn run(self, mut context: ServiceContext<TweetProtocol>) {
        let mut active: Option<ActiveSession<S::Session>> = None;
        let mut selection_avatars: HashMap<SmolStr, SmolStr> =
            HashMap::with_capacity(MAX_X_SELECTIONS);
        let mut search_avatars: Vec<SmolStr> = Vec::new();
        let mut selections_watched = false;

        while let Some(request) = context.next_rpc().await {
            let (mut reply, request) = request.into_parts();
            match request {
                TweetRequest::Stop => {
                    if let Some(active) = active.take() {
                        context.cancel_stream(&active.scope);
                    }
                    let _ = reply.respond(Ok(TweetResponse::Stopped));
                }
                request @ (TweetRequest::Load(_) | TweetRequest::Retry(_)) => {
                    let (scope, retry) = match request {
                        TweetRequest::Load(scope) => (scope, false),
                        TweetRequest::Retry(scope) => (scope, true),
                        TweetRequest::Stop
                        | TweetRequest::FetchSelections(_)
                        | TweetRequest::WatchSelections { .. }
                        | TweetRequest::SearchUsers { .. }
                        | TweetRequest::AddSelection { .. }
                        | TweetRequest::DeleteSelection { .. }
                        | TweetRequest::RetryMedia { .. } => continue,
                    };
                    if reply.deadline() <= Instant::now() || reply.is_cancelled() {
                        continue;
                    }

                    if !retry
                        && active.as_ref().is_some_and(|active| active.scope == scope)
                        && context.stream_generation(&scope).is_some()
                    {
                        let _ = reply.respond(Ok(TweetResponse::StreamStarted));
                        continue;
                    }

                    let acquire = self.source.acquire_session(scope.account_id());
                    tokio::pin!(acquire);
                    let session = tokio::select! {
                        biased;
                        () = reply.cancelled() => continue,
                        result = &mut acquire => match result {
                            Ok(session) => session,
                            Err(error) => {
                                let _ = reply.respond(Err(session_failure(error)));
                                continue;
                            }
                        },
                    };
                    if session.account_id() != scope.account_id() {
                        let _ = reply.respond(Err(XApiError::AccountChanged));
                        continue;
                    }

                    if let Some(previous) = active.take() {
                        context.cancel_stream(&previous.scope);
                    }
                    let streaming = session.clone();
                    let downloader = self.downloader.clone();
                    let wanted =
                        auxiliary_urls(&selection_avatars, &search_avatars, selections_watched);
                    let (auxiliary, auxiliary_changes) = watch::channel(wanted);
                    let (retries, retry_requests) = mpsc::channel(MEDIA_RETRY_CAPACITY);
                    context.start_stream(scope.clone(), move |publisher| async move {
                        run_stream(
                            streaming,
                            downloader,
                            publisher,
                            auxiliary_changes,
                            retry_requests,
                        )
                        .await;
                    });
                    active = Some(ActiveSession {
                        scope: scope.clone(),
                        session,
                        auxiliary,
                        retries,
                    });
                    if reply.respond(Ok(TweetResponse::StreamStarted)).is_err() {
                        context.cancel_stream(&scope);
                        active = None;
                    }
                }
                TweetRequest::FetchSelections(scope) => {
                    let session = match matching_session(&active, &scope) {
                        Ok(active) => active.session(),
                        Err(error) => {
                            let _ = reply.respond(Err(error));
                            continue;
                        }
                    };
                    let fetch = session.fetch_selections();
                    tokio::pin!(fetch);
                    let response = tokio::select! {
                        biased;
                        () = reply.cancelled() => continue,
                        response = &mut fetch => match response {
                            Ok(response) => response,
                            Err(error) => {
                                let _ = reply.respond(Err(error));
                                continue;
                            }
                        },
                    };
                    let data = SelectionData::new(response);
                    selection_avatars = data
                        .selections()
                        .iter()
                        .filter_map(|selection| {
                            let profile = selection.profile.as_ref()?;
                            (!profile.avatar_url.is_empty())
                                .then(|| (selection.x_user_id.clone(), profile.avatar_url.clone()))
                        })
                        .collect();
                    if let Ok(active) = matching_session(&active, &scope) {
                        let _ = active;
                    }
                    if let Some(active) = active.as_ref().filter(|active| active.scope == scope) {
                        active.auxiliary.send_replace(auxiliary_urls(
                            &selection_avatars,
                            &search_avatars,
                            selections_watched,
                        ));
                    }
                    let _ = reply.respond(Ok(TweetResponse::Selections(data)));
                }
                TweetRequest::WatchSelections { scope, watched } => {
                    let Some(active) = active.as_ref().filter(|active| active.scope == scope)
                    else {
                        let _ = reply.respond(Err(XApiError::NotReady));
                        continue;
                    };
                    selections_watched = watched;
                    active.auxiliary.send_replace(auxiliary_urls(
                        &selection_avatars,
                        &search_avatars,
                        selections_watched,
                    ));
                    let _ = reply.respond(Ok(TweetResponse::SelectionsWatched));
                }
                TweetRequest::SearchUsers { scope, query } => {
                    let session = match matching_session(&active, &scope) {
                        Ok(active) => active.session(),
                        Err(error) => {
                            let _ = reply.respond(Err(error));
                            continue;
                        }
                    };
                    let search = session.search_users(&query);
                    tokio::pin!(search);
                    let profiles = tokio::select! {
                        biased;
                        () = reply.cancelled() => continue,
                        result = &mut search => match result {
                            Ok(profiles) => profiles,
                            Err(error) => {
                                let _ = reply.respond(Err(error));
                                continue;
                            }
                        },
                    };
                    search_avatars = profiles
                        .iter()
                        .filter(|profile| !profile.avatar_url.is_empty())
                        .map(|profile| profile.avatar_url.clone())
                        .collect();
                    if let Some(active) = active.as_ref().filter(|active| active.scope == scope) {
                        active.auxiliary.send_replace(auxiliary_urls(
                            &selection_avatars,
                            &search_avatars,
                            selections_watched,
                        ));
                    }
                    let _ = reply.respond(Ok(TweetResponse::SearchResults(profiles.into())));
                }
                TweetRequest::AddSelection { scope, username } => {
                    let session = match matching_session(&active, &scope) {
                        Ok(active) => active.session(),
                        Err(error) => {
                            let _ = reply.respond(Err(error));
                            continue;
                        }
                    };
                    if !valid_x_username(&username) {
                        let _ = reply.respond(Ok(TweetResponse::Added(
                            XMutationOutcome::Rejected(XApiError::InvalidUsername),
                        )));
                        continue;
                    }
                    let add = session.add_selection(&username);
                    tokio::pin!(add);
                    let outcome = tokio::select! {
                        biased;
                        () = reply.cancelled() => continue,
                        outcome = &mut add => outcome,
                    };
                    if let XMutationOutcome::Confirmed(selection) = &outcome
                        && let Some(profile) = selection.profile.as_ref()
                        && !profile.avatar_url.is_empty()
                    {
                        selection_avatars
                            .insert(selection.x_user_id.clone(), profile.avatar_url.clone());
                        if let Some(active) = active.as_ref().filter(|active| active.scope == scope)
                        {
                            active.auxiliary.send_replace(auxiliary_urls(
                                &selection_avatars,
                                &search_avatars,
                                selections_watched,
                            ));
                        }
                    }
                    let _ = reply.respond(Ok(TweetResponse::Added(outcome)));
                }
                TweetRequest::DeleteSelection { scope, x_user_id } => {
                    let session = match matching_session(&active, &scope) {
                        Ok(active) => active.session(),
                        Err(error) => {
                            let _ = reply.respond(Err(error));
                            continue;
                        }
                    };
                    if !valid_x_id(&x_user_id) {
                        let _ =
                            reply.respond(Ok(TweetResponse::Deleted(XMutationOutcome::Rejected(
                                XApiError::Protocol("X user ID is invalid".into()),
                            ))));
                        continue;
                    }
                    let delete = session.delete_selection(&x_user_id);
                    tokio::pin!(delete);
                    let outcome = tokio::select! {
                        biased;
                        () = reply.cancelled() => continue,
                        outcome = &mut delete => outcome,
                    };
                    if matches!(&outcome, XMutationOutcome::Confirmed(())) {
                        selection_avatars.remove(&x_user_id);
                        if let Some(active) = active.as_ref().filter(|active| active.scope == scope)
                        {
                            active.auxiliary.send_replace(auxiliary_urls(
                                &selection_avatars,
                                &search_avatars,
                                selections_watched,
                            ));
                        }
                    }
                    let _ = reply.respond(Ok(TweetResponse::Deleted(outcome)));
                }
                TweetRequest::RetryMedia { scope, url } => {
                    let active = match matching_session(&active, &scope) {
                        Ok(active) => active,
                        Err(error) => {
                            let _ = reply.respond(Err(error));
                            continue;
                        }
                    };
                    if !super::media::plausible_media_url(&url) {
                        let _ = reply
                            .respond(Err(XApiError::Protocol("X media URL is invalid".into())));
                        continue;
                    }
                    if active.retries.try_send(url).is_err() {
                        let _ = reply.respond(Err(XApiError::NotReady));
                        continue;
                    }
                    let _ = reply.respond(Ok(TweetResponse::MediaRetried));
                }
            }
        }
    }
}

fn auxiliary_urls(
    selections: &HashMap<SmolStr, SmolStr>,
    search: &[SmolStr],
    watched: bool,
) -> Arc<[SmolStr]> {
    let mut urls = Vec::with_capacity(selections.len() + search.len());
    if watched {
        urls.extend(selections.values().cloned());
    }
    urls.extend(search.iter().cloned());
    urls.into()
}

fn matching_session<'a, S>(
    active: &'a Option<ActiveSession<S>>,
    scope: &TweetScope,
) -> Result<&'a ActiveSession<S>, XApiError> {
    let Some(active) = active.as_ref() else {
        return Err(XApiError::NotReady);
    };
    if &active.scope != scope {
        return Err(XApiError::AccountChanged);
    }
    Ok(active)
}

fn session_failure(error: KytrixServiceError) -> XApiError {
    match error {
        KytrixServiceError::Authentication(_) => XApiError::Authentication,
        KytrixServiceError::AccountChanged => XApiError::AccountChanged,
        KytrixServiceError::Timeout => XApiError::Timeout,
        KytrixServiceError::Closed => XApiError::Closed,
        error => XApiError::Service(error),
    }
}

async fn run_stream<S, T>(
    session: S,
    downloader: MediaDownloader<T>,
    publisher: StreamPublisher<TweetProtocol>,
    wanted: watch::Receiver<Arc<[SmolStr]>>,
    retries: mpsc::Receiver<SmolStr>,
) where
    S: TweetSession,
    T: MediaTransport,
{
    if publish_status(&publisher, TweetStatus::Connecting)
        .await
        .is_err()
    {
        return;
    }

    let mut media = MediaSet::new(downloader, CancellationToken::new());
    let exit = serve(session, &publisher, wanted, retries, &mut media).await;
    media.shutdown().await;
    if let StreamExit::Failed(error) = exit {
        publish_failure(&publisher, error).await;
    }
}

async fn serve<S, T>(
    session: S,
    publisher: &StreamPublisher<TweetProtocol>,
    mut wanted: watch::Receiver<Arc<[SmolStr]>>,
    mut retries: mpsc::Receiver<SmolStr>,
    media: &mut MediaSet<T>,
) -> StreamExit
where
    S: TweetSession,
    T: MediaTransport,
{
    let mut published = TweetStatus::Connecting;
    let subscription = tokio::select! {
        biased;
        () = publisher.cancelled() => return StreamExit::Cancelled,
        result = session.subscribe_x() => result,
    };
    let mut subscription = match subscription {
        Ok(subscription) => subscription,
        Err(error) => {
            return StreamExit::Failed(TweetStreamError::Service(error));
        }
    };

    let mut status = subscription.observe_status();
    let mut state = TweetState::new();
    let mut revision = 0;

    let history = tokio::select! {
        biased;
        () = publisher.cancelled() => return StreamExit::Cancelled,
        result = session.fetch_tweets() => result,
    };
    let batch = match history {
        Ok(batch) => batch,
        Err(error) => {
            return StreamExit::Failed(TweetStreamError::Api(error));
        }
    };
    let current = status.borrow_and_update().clone();
    if let Some(error) = ended_subscription(&current) {
        return StreamExit::Failed(error);
    }
    accept_history(&mut state, batch);
    media.reconcile(state.media_urls());
    media.fill();
    if publish_update(publisher, &mut state, media, &mut revision).is_err() {
        return StreamExit::Cancelled;
    }
    if !apply_subscription_status(publisher, &current, &mut published).await {
        return StreamExit::Cancelled;
    }

    run_subscription(
        &mut subscription,
        &mut status,
        &mut state,
        publisher,
        &mut wanted,
        &mut retries,
        media,
        &mut revision,
        &mut published,
    )
    .await
}

fn accept_history(state: &mut TweetState, batch: XTweetBatch) {
    for tweet in batch.tweets {
        state.push(tweet);
    }
    state.prune_profiles();
    for profile in batch.profiles {
        state.upsert_profile(profile);
    }
    state.prune_profiles();
}

async fn run_subscription<Sub, T>(
    subscription: &mut Sub,
    status: &mut watch::Receiver<SubscriptionStatus>,
    state: &mut TweetState,
    publisher: &StreamPublisher<TweetProtocol>,
    wanted: &mut watch::Receiver<Arc<[SmolStr]>>,
    retries: &mut mpsc::Receiver<SmolStr>,
    media: &mut MediaSet<T>,
    revision: &mut u64,
    published: &mut TweetStatus,
) -> StreamExit
where
    Sub: TweetEventSubscription,
    T: MediaTransport,
{
    let mut retries_open = true;
    loop {
        media.fill();
        let input = tokio::select! {
            biased;
            () = publisher.cancelled() => StreamInput::Cancelled,
            changed = status.changed() => StreamInput::Status(changed.is_err()),
            event = subscription.recv() => StreamInput::Event(event),
            changed = wanted.changed() => StreamInput::Wanted(changed.is_ok()),
            retry = retries.recv(), if retries_open => match retry {
                Some(url) => StreamInput::Retry(url),
                None => {
                    retries_open = false;
                    continue;
                }
            },
            finished = media.active.join_next_with_id(), if !media.active.is_empty() => {
                match finished {
                    Some(finished) => StreamInput::Media(finished),
                    None => continue,
                }
            }
        };
        match input {
            StreamInput::Cancelled => return StreamExit::Cancelled,
            StreamInput::Status(closed) => {
                let next = if closed {
                    SubscriptionStatus::Closed
                } else {
                    status.borrow_and_update().clone()
                };
                if !apply_subscription_status(publisher, &next, published).await {
                    return StreamExit::Cancelled;
                }
            }
            StreamInput::Wanted(false) => return StreamExit::Cancelled,
            StreamInput::Wanted(true) => {
                let urls = wanted.borrow_and_update().clone();
                if media.replace_auxiliary(urls)
                    && publish_update(publisher, state, media, revision).is_err()
                {
                    return StreamExit::Cancelled;
                }
            }
            StreamInput::Retry(url) => {
                if media.retry(&url) && publish_update(publisher, state, media, revision).is_err() {
                    return StreamExit::Cancelled;
                }
            }
            StreamInput::Media(finished) => {
                if media.finish(finished)
                    && publish_update(publisher, state, media, revision).is_err()
                {
                    return StreamExit::Cancelled;
                }
            }
            StreamInput::Event(event) => {
                if publisher.is_cancelled() {
                    return StreamExit::Cancelled;
                }
                let current = status.borrow_and_update().clone();
                if let Some(error) = ended_subscription(&current) {
                    return StreamExit::Failed(error);
                }
                let event = match event {
                    Ok(event) => event,
                    Err(error) => return StreamExit::Failed(TweetStreamError::Transport(error)),
                };
                let changed = match event {
                    XSubscriptionEvent::Profile(profile) => state.upsert_profile(profile),
                    XSubscriptionEvent::XTweet(tweet) => state.push(tweet),
                };
                let pruned = state.prune_profiles();
                if !changed && !pruned {
                    continue;
                }
                if changed {
                    media.reconcile(state.media_urls());
                }
                if publish_update(publisher, state, media, revision).is_err() {
                    return StreamExit::Cancelled;
                }
            }
        }
    }
}

enum StreamInput {
    Cancelled,
    Wanted(bool),
    Status(bool),
    Event(Result<XSubscriptionEvent, TransportError>),
    Retry(SmolStr),
    Media(MediaTaskResult),
}

type MediaTaskResult =
    Result<(tokio::task::Id, (SmolStr, Result<PathBuf, MediaError>)), tokio::task::JoinError>;

enum StreamExit {
    Cancelled,
    Failed(TweetStreamError),
}

fn publish_update<T>(
    publisher: &StreamPublisher<TweetProtocol>,
    state: &mut TweetState,
    media: &MediaSet<T>,
    revision: &mut u64,
) -> Result<(), StreamPublishError>
where
    T: MediaTransport,
{
    *revision = revision.saturating_add(1);
    publisher.publish_latest(TweetStreamMessage::Update(update(*revision, state, media)))
}

fn update<T>(revision: u64, state: &mut TweetState, media: &MediaSet<T>) -> TweetUpdate
where
    T: MediaTransport,
{
    TweetUpdate {
        revision,
        feed: state.snapshot(),
        media: media.states(),
    }
}

async fn apply_subscription_status(
    publisher: &StreamPublisher<TweetProtocol>,
    status: &SubscriptionStatus,
    published: &mut TweetStatus,
) -> bool {
    if let Some(error) = ended_subscription(status) {
        publish_failure(publisher, error).await;
        return false;
    }
    match status {
        SubscriptionStatus::Closing => true,
        SubscriptionStatus::Connecting | SubscriptionStatus::Reconnecting => {
            publish_change(publisher, TweetStatus::Connecting, published)
                .await
                .is_ok()
        }
        SubscriptionStatus::Active => publish_change(publisher, TweetStatus::Ready, published)
            .await
            .is_ok(),
        SubscriptionStatus::Closed | SubscriptionStatus::Failed(_) => false,
    }
}

fn ended_subscription(status: &SubscriptionStatus) -> Option<TweetStreamError> {
    match status {
        SubscriptionStatus::Closed => Some(TweetStreamError::Transport(TransportError::Closed)),
        SubscriptionStatus::Failed(error) => Some(TweetStreamError::Transport(error.clone())),
        SubscriptionStatus::Connecting
        | SubscriptionStatus::Active
        | SubscriptionStatus::Reconnecting
        | SubscriptionStatus::Closing => None,
    }
}

async fn publish_status(
    publisher: &StreamPublisher<TweetProtocol>,
    status: TweetStatus,
) -> Result<(), StreamPublishError> {
    publisher
        .publish_reliable(TweetStreamMessage::Status(status))
        .await
}

async fn publish_change(
    publisher: &StreamPublisher<TweetProtocol>,
    status: TweetStatus,
    published: &mut TweetStatus,
) -> Result<(), StreamPublishError> {
    if *published == status {
        return Ok(());
    }
    publish_status(publisher, status.clone()).await?;
    *published = status;
    Ok(())
}

async fn publish_failure(publisher: &StreamPublisher<TweetProtocol>, error: TweetStreamError) {
    let _ = publish_status(publisher, TweetStatus::Failed(error)).await;
    publisher.cancelled().await;
}

struct MediaSet<T> {
    downloader: MediaDownloader<T>,
    cancellation: CancellationToken,
    states: HashMap<SmolStr, MediaState>,
    timeline: HashSet<SmolStr>,
    auxiliary: HashSet<SmolStr>,
    pending: VecDeque<SmolStr>,
    active: JoinSet<(SmolStr, Result<PathBuf, MediaError>)>,
    jobs: HashMap<tokio::task::Id, SmolStr>,
}

impl<T> MediaSet<T>
where
    T: MediaTransport,
{
    fn new(downloader: MediaDownloader<T>, cancellation: CancellationToken) -> Self {
        Self {
            downloader,
            cancellation,
            states: HashMap::new(),
            timeline: HashSet::new(),
            auxiliary: HashSet::new(),
            pending: VecDeque::new(),
            active: JoinSet::new(),
            jobs: HashMap::new(),
        }
    }

    fn reconcile(&mut self, urls: Vec<SmolStr>) -> bool {
        self.timeline = urls.into_iter().collect();
        self.refresh()
    }

    fn replace_auxiliary(&mut self, urls: Arc<[SmolStr]>) -> bool {
        self.auxiliary = urls.iter().cloned().collect();
        self.refresh()
    }

    fn refresh(&mut self) -> bool {
        let mut wanted = HashSet::with_capacity(self.timeline.len() + self.auxiliary.len());
        wanted.extend(self.timeline.iter().cloned());
        wanted.extend(self.auxiliary.iter().cloned());

        let mut changed = false;
        let known = self.states.len();
        self.states.retain(|url, _| wanted.contains(url));
        changed |= self.states.len() != known;
        self.pending.retain(|url| wanted.contains(url));

        for url in wanted {
            if !self.states.contains_key(&url) {
                self.states.insert(url.clone(), MediaState::Loading);
                self.pending.push_back(url);
                changed = true;
            }
        }
        changed
    }

    fn retry(&mut self, url: &str) -> bool {
        if self.states.get(url) != Some(&MediaState::Failed) {
            return false;
        }
        self.states.insert(SmolStr::new(url), MediaState::Loading);
        self.pending.push_back(SmolStr::new(url));
        true
    }

    fn fill(&mut self) {
        while self.active.len() < MEDIA_CONCURRENCY {
            let Some(url) = self.pending.pop_front() else {
                break;
            };
            if !self.states.contains_key(&url) {
                continue;
            }
            let downloader = self.downloader.clone();
            let cancellation = self.cancellation.clone();
            let job = url.clone();
            let task = self.active.spawn(async move {
                let deadline = Instant::now() + MEDIA_OPERATION_TIMEOUT;
                let result = downloader.fetch(&job, deadline, &cancellation).await;
                (job, result)
            });
            self.jobs.insert(task.id(), url);
        }
    }

    fn finish(&mut self, result: MediaTaskResult) -> bool {
        let (id, outcome) = match result {
            Ok((id, outcome)) => (id, Some(outcome)),
            Err(error) => (error.id(), None),
        };
        let Some(known) = self.jobs.get(&id) else {
            return false;
        };
        let url = match outcome.as_ref() {
            Some((url, _)) => url.clone(),
            None => {
                log::debug!("a picture of {known} did not finish");
                known.clone()
            }
        };
        self.jobs.remove(&id);
        if !self.states.contains_key(&url) {
            return false;
        }
        let state = match outcome {
            Some((_, Ok(path))) => MediaState::Ready(path),
            Some((_, Err(error))) => {
                log::debug!("the picture of {url} could not be fetched: {error}");
                MediaState::Failed
            }
            None => MediaState::Failed,
        };
        self.states.insert(url, state);
        true
    }

    fn states(&self) -> MediaStates {
        Arc::new(self.states.clone())
    }

    async fn shutdown(&mut self) {
        self.cancellation.cancel();
        while self.active.join_next().await.is_some() {}
    }
}

#[cfg(test)]
mod tests {

    #[allow(
        clippy::disallowed_methods,
        reason = "a test drives its subject on its own runtime so the paused clock it advances is the one the subject reads"
    )]
    fn current_test_handle() -> &'static tokio::runtime::Handle {
        Box::leak(Box::new(tokio::runtime::Handle::current()))
    }
    use std::time::Duration;

    use widget::{
        rpc::RpcCallError,
        runtime::WidgetRuntime,
        stream::{LatestStream, ReliableStream},
    };

    use super::*;
    use crate::{
        media::MediaCache,
        model::MAX_TWEETS,
        test_support::{
            ACCOUNT, FakeSession, FakeSource, FakeTransport, media_cache, media_url, profile,
            search_profile, seed_picture, selection, selections, tweet, tweet_with_media,
        },
    };

    fn account(name: &str) -> TweetScope {
        TweetScope::new(Some(&SmolStr::new(name))).expect("an account should be a scope")
    }

    fn scope() -> TweetScope {
        account(ACCOUNT)
    }

    fn runtime(
        source: &FakeSource,
        transport: FakeTransport,
        cache: MediaCache,
    ) -> WidgetRuntime<TweetProtocol> {
        WidgetRuntime::spawn(
            current_test_handle(),
            TweetService::new(
                source.clone(),
                MediaDownloader::with_cache(transport, cache),
            ),
        )
    }

    async fn load(runtime: &WidgetRuntime<TweetProtocol>) {
        let response = runtime
            .client()
            .call(TweetRequest::Load(scope()))
            .await
            .expect("the stream should start")
            .into_value();
        assert_eq!(response, TweetResponse::StreamStarted);
    }

    async fn fetch(
        runtime: &WidgetRuntime<TweetProtocol>,
        scope: TweetScope,
    ) -> Result<TweetResponse, RpcCallError<XApiError>> {
        runtime
            .client()
            .call(TweetRequest::FetchSelections(scope))
            .await
            .map(widget::rpc::RpcResponse::into_value)
    }

    async fn call(
        runtime: &WidgetRuntime<TweetProtocol>,
        request: TweetRequest,
    ) -> Result<TweetResponse, RpcCallError<XApiError>> {
        runtime
            .client()
            .call(request)
            .await
            .map(widget::rpc::RpcResponse::into_value)
    }

    fn usernames(response: &TweetResponse) -> Vec<String> {
        let TweetResponse::Selections(data) = response else {
            panic!("the answer should be the monitored accounts");
        };
        data.selections()
            .iter()
            .map(|selection| {
                selection.profile.as_ref().map_or_else(
                    || selection.x_user_id.to_string(),
                    |p| p.username.to_string(),
                )
            })
            .collect()
    }

    async fn next_reliable(
        reliable: &mut ReliableStream<TweetProtocol>,
        predicate: impl Fn(&TweetStreamMessage) -> bool,
    ) -> TweetStreamMessage {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let envelope = reliable
                    .recv()
                    .await
                    .expect("the reliable stream should stay open");
                if predicate(envelope.message().as_ref()) {
                    return envelope.message().as_ref().clone();
                }
            }
        })
        .await
        .expect("the expected reliable state should arrive")
    }

    async fn receive_status(reliable: &mut ReliableStream<TweetProtocol>, expected: TweetStatus) {
        let status = next_reliable(
            reliable,
            |message| matches!(message, TweetStreamMessage::Status(status) if *status == expected),
        )
        .await;
        assert!(matches!(status, TweetStreamMessage::Status(_)));
    }

    async fn receive_failure(reliable: &mut ReliableStream<TweetProtocol>) -> TweetStreamError {
        let failure = next_reliable(reliable, |message| {
            matches!(message, TweetStreamMessage::Status(TweetStatus::Failed(_)))
        })
        .await;
        let TweetStreamMessage::Status(TweetStatus::Failed(error)) = failure else {
            panic!("the stream should have reported a failure");
        };
        error
    }

    async fn next_update(
        latest: &mut LatestStream<TweetProtocol>,
        predicate: impl Fn(&TweetUpdate) -> bool,
    ) -> TweetUpdate {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                latest
                    .changed()
                    .await
                    .expect("the latest stream should stay open");
                while let Some(envelope) = latest.next_unseen() {
                    if let TweetStreamMessage::Update(update) = envelope.message().as_ref()
                        && predicate(update)
                    {
                        return update.clone();
                    }
                }
            }
        })
        .await
        .expect("the expected update should arrive")
    }

    fn contents(update: &TweetUpdate) -> Vec<String> {
        update
            .feed()
            .items()
            .iter()
            .map(|item| item.tweet_id().to_owned())
            .collect()
    }

    fn picture(update: &TweetUpdate, url: &str) -> Option<MediaState> {
        update.media().get(url).cloned()
    }

    async fn live(
        source: &FakeSource,
        transport: FakeTransport,
        cache: MediaCache,
    ) -> (
        WidgetRuntime<TweetProtocol>,
        ReliableStream<TweetProtocol>,
        LatestStream<TweetProtocol>,
    ) {
        let mut runtime = runtime(source, transport, cache);
        let reliable = runtime
            .take_reliable_stream()
            .expect("the reliable stream should be available");
        let latest = runtime.latest_stream();
        load(&runtime).await;
        (runtime, reliable, latest)
    }

    async fn subscribed(
        transport: FakeTransport,
        cache: MediaCache,
    ) -> (
        FakeSource,
        FakeSession,
        crate::test_support::FakeSubscriptionControl,
        WidgetRuntime<TweetProtocol>,
        ReliableStream<TweetProtocol>,
        LatestStream<TweetProtocol>,
    ) {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        let (runtime, reliable, latest) = live(&source, transport, cache).await;
        (source, session, control, runtime, reliable, latest)
    }

    #[tokio::test]
    async fn the_stream_should_publish_history_and_call_itself_live() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        session.answer_history(
            vec![tweet("1", 1), tweet("2", 2)],
            vec![profile("42", "kytrix")],
        );
        let (_runtime, mut reliable, mut latest) =
            live(&source, FakeTransport::default(), media_cache().0).await;

        receive_status(&mut reliable, TweetStatus::Ready).await;
        let update = latest
            .next_unseen()
            .expect("a live timeline is published with the readiness it comes with");
        let TweetStreamMessage::Update(update) = update.message().as_ref() else {
            panic!("the latest stream should hold the timeline");
        };
        assert_eq!(contents(update), ["2", "1"]);

        control
            .events
            .send(Ok(XSubscriptionEvent::XTweet(tweet("3", 3))))
            .await
            .expect("the live subscription should accept a tweet");
        let update = next_update(&mut latest, |update| contents(update).len() == 3).await;
        assert_eq!(contents(&update), ["3", "2", "1"]);
    }

    #[tokio::test]
    async fn a_history_that_could_not_be_read_should_fail_the_stream() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        session.subscribe();
        session.refuse_history(XApiError::Closed);

        let (_runtime, mut reliable, _) =
            live(&source, FakeTransport::default(), media_cache().0).await;

        assert_eq!(
            receive_failure(&mut reliable).await,
            TweetStreamError::Api(XApiError::Closed)
        );
    }

    #[tokio::test]
    async fn a_subscription_that_could_not_be_taken_should_fail_the_stream() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        session.refuse_subscription(KytrixServiceError::Timeout);

        let (_runtime, mut reliable, _) =
            live(&source, FakeTransport::default(), media_cache().0).await;

        assert_eq!(
            receive_failure(&mut reliable).await,
            TweetStreamError::Service(KytrixServiceError::Timeout)
        );
    }

    #[tokio::test]
    async fn an_account_the_pane_has_left_should_be_refused_rather_than_answered() {
        let (_, session, _control, runtime, _reliable, _latest) =
            subscribed(FakeTransport::default(), media_cache().0).await;
        session.answer_selections(selections(vec![selection("1", "one")], 3));

        let refused = fetch(&runtime, account("another-account")).await;

        match refused.expect_err("another account is not this one") {
            RpcCallError::Service { source, .. } => {
                assert_eq!(source, XApiError::AccountChanged);
            }
            other => panic!("the refusal should be the account, not {other:?}"),
        }
        let counts = session.state();
        assert_eq!(counts.history_reads, 1);
        assert_eq!(counts.fetches, 0);
    }

    #[tokio::test]
    async fn a_read_should_be_answered_with_the_accounts_the_service_was_given() {
        let (_, session, _control, runtime, _reliable, _latest) =
            subscribed(FakeTransport::default(), media_cache().0).await;
        session.answer_selections(selections(
            vec![selection("1", "one"), selection("2", "two")],
            5,
        ));

        let response = fetch(&runtime, scope())
            .await
            .expect("the read should answer");

        assert_eq!(usernames(&response), ["one", "two"]);
        let TweetResponse::Selections(data) = response else {
            panic!("the answer should be the monitored accounts");
        };
        assert_eq!(data.limit(), 5);
        assert!(!data.quota_full());
    }

    #[tokio::test]
    async fn a_change_should_be_answered_with_what_became_of_it() {
        let (_, session, _control, runtime, _reliable, _latest) =
            subscribed(FakeTransport::default(), media_cache().0).await;
        session.answer_add(XMutationOutcome::Confirmed(selection("1", "one")));
        session.answer_delete(XMutationOutcome::Confirmed(()));

        let added = call(
            &runtime,
            TweetRequest::AddSelection {
                scope: scope(),
                username: SmolStr::new_static("one"),
            },
        )
        .await
        .expect("the change should be answered");
        assert!(matches!(
            added,
            TweetResponse::Added(XMutationOutcome::Confirmed(_))
        ));

        let deleted = call(
            &runtime,
            TweetRequest::DeleteSelection {
                scope: scope(),
                x_user_id: SmolStr::new_static("1"),
            },
        )
        .await
        .expect("the change should be answered");
        assert_eq!(
            deleted,
            TweetResponse::Deleted(XMutationOutcome::Confirmed(()))
        );
    }

    #[tokio::test]
    async fn a_change_that_is_not_one_should_be_refused_without_asking() {
        let (_, session, _control, runtime, _reliable, _latest) =
            subscribed(FakeTransport::default(), media_cache().0).await;

        let added = call(
            &runtime,
            TweetRequest::AddSelection {
                scope: scope(),
                username: SmolStr::new_static("not a username"),
            },
        )
        .await
        .expect("the refusal should be an answer");
        assert_eq!(
            added,
            TweetResponse::Added(XMutationOutcome::Rejected(XApiError::InvalidUsername))
        );

        let deleted = call(
            &runtime,
            TweetRequest::DeleteSelection {
                scope: scope(),
                x_user_id: SmolStr::new_static("not an id"),
            },
        )
        .await
        .expect("the refusal should be an answer");
        assert!(matches!(
            deleted,
            TweetResponse::Deleted(XMutationOutcome::Rejected(XApiError::Protocol(_)))
        ));
        assert_eq!(session.state().add_calls, 0);
        assert_eq!(session.state().delete_calls, 0);
    }

    #[tokio::test]
    async fn a_search_should_be_answered_and_its_results_be_the_pictures_to_fetch() {
        let (_, session, _control, runtime, _reliable, _latest) =
            subscribed(FakeTransport::failing(), media_cache().0).await;
        session.answer_search(vec![search_profile("7", "seven")]);

        let response = call(
            &runtime,
            TweetRequest::SearchUsers {
                scope: scope(),
                query: SmolStr::new_static("seven"),
            },
        )
        .await
        .expect("the search should be answered");

        let TweetResponse::SearchResults(profiles) = response else {
            panic!("the answer should be the search results");
        };
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].username, "seven");
    }

    #[tokio::test]
    async fn the_monitored_avatars_should_be_fetched_only_while_a_dialog_shows_them() {
        let (cache, _root) = media_cache();
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        session.answer_history(vec![tweet("1", 1)], vec![profile("42", "kytrix")]);
        session.answer_selections(selections(vec![selection("7", "seven")], 3));
        let (runtime, _reliable, mut latest) =
            live(&source, FakeTransport::blocking(), cache).await;
        let avatar = "https://pbs.twimg.com/seven.jpg";

        next_update(&mut latest, |update| {
            picture(update, "https://pbs.twimg.com/kytrix.jpg").is_some()
        })
        .await;

        fetch(&runtime, scope())
            .await
            .expect("the read should answer");
        control
            .events
            .send(Ok(XSubscriptionEvent::XTweet(tweet("2", 2))))
            .await
            .expect("the subscription should accept a tweet");
        let update = next_update(&mut latest, |update| contents(update).len() == 2).await;
        assert_eq!(picture(&update, avatar), None);

        call(
            &runtime,
            TweetRequest::WatchSelections {
                scope: scope(),
                watched: true,
            },
        )
        .await
        .expect("watching should be acknowledged");
        let update = next_update(&mut latest, |update| picture(update, avatar).is_some()).await;
        assert!(
            matches!(picture(&update, avatar), Some(MediaState::Loading)),
            "a watched avatar is a picture the stream is fetching"
        );

        call(
            &runtime,
            TweetRequest::WatchSelections {
                scope: scope(),
                watched: false,
            },
        )
        .await
        .expect("watching should be acknowledged");
        let update = next_update(&mut latest, |update| {
            picture(update, avatar).is_none()
                && picture(update, "https://pbs.twimg.com/kytrix.jpg").is_some()
        })
        .await;
        assert!(contents(&update).contains(&"1".to_owned()));
    }

    #[tokio::test]
    async fn a_picture_on_disk_should_be_published_as_ready_without_a_fetch() {
        let (cache, _root) = media_cache();
        let url = media_url("one");
        let path = seed_picture(&cache, &url, b"image").await;
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        session.answer_history(
            vec![tweet_with_media("1", 1, url.as_str())],
            vec![profile("42", "kytrix")],
        );
        let (_runtime, _reliable, mut latest) =
            live(&source, FakeTransport::default(), cache).await;

        let update = next_update(&mut latest, |update| {
            picture(update, url.as_str()) == Some(MediaState::Ready(path.clone()))
        })
        .await;

        assert_eq!(contents(&update), ["1"]);
        let _ = control;
    }

    #[tokio::test]
    async fn a_picture_that_could_not_be_fetched_should_be_published_as_failed() {
        let (cache, _root) = media_cache();
        let url = media_url("two");
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        session.answer_history(
            vec![tweet_with_media("1", 1, url.as_str())],
            vec![profile("42", "kytrix")],
        );
        let (_runtime, _reliable, mut latest) =
            live(&source, FakeTransport::failing(), cache).await;

        let update = next_update(&mut latest, |update| {
            picture(update, url.as_str()) == Some(MediaState::Failed)
        })
        .await;

        assert_eq!(contents(&update), ["1"]);
        let _ = control;
    }

    #[tokio::test]
    async fn a_retry_should_be_refused_for_a_picture_no_tweet_wants() {
        let (_, _session, _control, runtime, _reliable, _latest) =
            subscribed(FakeTransport::default(), media_cache().0).await;

        let refused = call(
            &runtime,
            TweetRequest::RetryMedia {
                scope: scope(),
                url: SmolStr::new_static("https://example.com/not-a-picture.jpg"),
            },
        )
        .await;

        match refused.expect_err("a URL this pane may not fetch is not a picture") {
            RpcCallError::Service { source, .. } => {
                assert!(matches!(source, XApiError::Protocol(_)));
            }
            other => panic!("the refusal should be a protocol error, not {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_failed_picture_should_be_fetched_again_when_the_pane_asks() {
        let (cache, _root) = media_cache();
        let url = media_url("three");
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        session.answer_history(
            vec![tweet_with_media("1", 1, url.as_str())],
            vec![profile("42", "kytrix")],
        );
        let (runtime, _reliable, mut latest) =
            live(&source, FakeTransport::failing(), cache.clone()).await;
        next_update(&mut latest, |update| {
            picture(update, url.as_str()) == Some(MediaState::Failed)
        })
        .await;

        seed_picture(&cache, &url, b"image").await;
        call(
            &runtime,
            TweetRequest::RetryMedia {
                scope: scope(),
                url: SmolStr::new(url.as_str()),
            },
        )
        .await
        .expect("the retry should be accepted");

        next_update(&mut latest, |update| {
            matches!(picture(update, url.as_str()), Some(MediaState::Ready(_)))
        })
        .await;
        let _ = control;
    }

    #[tokio::test]
    async fn a_download_nobody_wants_any_more_should_be_forgotten() {
        let (cache, _root) = media_cache();
        let url = media_url("four");
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        session.answer_history(
            vec![tweet_with_media("1", 1, url.as_str())],
            vec![profile("42", "kytrix")],
        );
        let (_runtime, _reliable, mut latest) =
            live(&source, FakeTransport::blocking(), cache).await;
        next_update(&mut latest, |update| {
            picture(update, url.as_str()) == Some(MediaState::Loading)
        })
        .await;

        for index in 2..MAX_TWEETS + 2 {
            control
                .events
                .send(Ok(XSubscriptionEvent::XTweet(tweet(
                    &index.to_string(),
                    index as i64,
                ))))
                .await
                .expect("the subscription should accept a tweet");
        }
        let update = next_update(&mut latest, |update| {
            !contents(update).contains(&"1".to_owned())
        })
        .await;

        assert_eq!(picture(&update, url.as_str()), None);
    }

    #[tokio::test]
    async fn a_replaced_stream_should_let_go_of_its_subscription() {
        let (source, session, control, runtime, _reliable, _latest) =
            subscribed(FakeTransport::default(), media_cache().0).await;

        session.answer_history(Vec::new(), vec![profile("42", "kytrix")]);
        session.subscribe();
        let replacement = source.session(ACCOUNT);
        replacement.answer_history(Vec::new(), vec![profile("42", "kytrix")]);
        replacement.subscribe();
        call(&runtime, TweetRequest::Retry(scope()))
            .await
            .expect("the retry should start a stream");

        tokio::time::timeout(Duration::from_secs(2), async {
            while control.drops.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the replaced subscription should be dropped");
    }

    #[tokio::test]
    async fn stopping_should_let_go_of_the_account_and_answer() {
        let (_, _session, control, runtime, _reliable, _latest) =
            subscribed(FakeTransport::default(), media_cache().0).await;

        let stopped = call(&runtime, TweetRequest::Stop)
            .await
            .expect("stopping should be answered");
        assert_eq!(stopped, TweetResponse::Stopped);

        tokio::time::timeout(Duration::from_secs(2), async {
            while control.drops.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the stopped subscription should be dropped");
    }

    #[tokio::test]
    async fn a_subscription_that_reconnects_should_report_itself_connecting_again() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        session.answer_history(Vec::new(), vec![profile("42", "kytrix")]);
        let (_runtime, mut reliable, _latest) =
            live(&source, FakeTransport::default(), media_cache().0).await;
        receive_status(&mut reliable, TweetStatus::Ready).await;

        control
            .status
            .send_replace(SubscriptionStatus::Reconnecting);
        receive_status(&mut reliable, TweetStatus::Connecting).await;

        control.status.send_replace(SubscriptionStatus::Active);
        receive_status(&mut reliable, TweetStatus::Ready).await;
    }

    #[tokio::test]
    async fn a_credential_change_already_on_the_wire_should_end_the_stream() {
        let source = FakeSource::default();
        let session = source.session(ACCOUNT);
        let control = session.subscribe();
        session.answer_history(Vec::new(), vec![profile("42", "kytrix")]);
        let (_runtime, mut reliable, _latest) =
            live(&source, FakeTransport::default(), media_cache().0).await;
        receive_status(&mut reliable, TweetStatus::Ready).await;

        control.fail_on_delivery(TransportError::CredentialChanged);
        control
            .events
            .send(Ok(XSubscriptionEvent::XTweet(tweet("1", 1))))
            .await
            .expect("the subscription should accept a tweet");

        assert_eq!(
            receive_failure(&mut reliable).await,
            TweetStreamError::Transport(TransportError::CredentialChanged)
        );
    }

    #[tokio::test]
    async fn an_acquisition_nobody_waits_for_any_more_should_be_dropped() {
        let source = FakeSource::default();
        let block = source.block();
        let runtime = runtime(&source, FakeTransport::default(), media_cache().0);
        let client = runtime.client();
        let call = tokio::spawn(async move { client.call(TweetRequest::Load(scope())).await });

        block.wait_started().await;
        call.abort();
        block.wait_dropped().await;
    }
}
