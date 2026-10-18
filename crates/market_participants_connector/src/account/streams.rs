//! Authenticated account streams.
//!
//! An order and a position event carries the listing the venue resolved it to;
//! this layer attaches that listing's units to the raw numbers, so a caller never
//! receives a quantity whose meaning it would have to guess.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures::Stream;

use super::{
    domain::{Order, Position},
    error::{AccountError, account_error_from_catalog, account_error_from_market_participant},
};
use crate::{
    adapter::{ProtocolEvent, SubscriptionHandle},
    catalog::resolved::EnsuredRoute,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountSubscriptionStatus {
    Connecting,
    Active,
    Reconnecting,
    Closed,
    Failed(AccountError),
}

impl From<crate::adapter::SubscriptionStatus> for AccountSubscriptionStatus {
    fn from(status: crate::adapter::SubscriptionStatus) -> Self {
        match status {
            crate::adapter::SubscriptionStatus::Connecting => Self::Connecting,
            crate::adapter::SubscriptionStatus::Active => Self::Active,
            crate::adapter::SubscriptionStatus::Reconnecting => Self::Reconnecting,
            crate::adapter::SubscriptionStatus::Closed => Self::Closed,
            crate::adapter::SubscriptionStatus::Failed(error) => {
                Self::Failed(account_error_from_market_participant(error))
            }
        }
    }
}

pub struct OrderStream {
    inner: crate::adapter::ProtocolStream,
    status: tokio::sync::watch::Receiver<AccountSubscriptionStatus>,
    catalog: EnsuredRoute,
}

pub struct PositionStream {
    inner: crate::adapter::ProtocolStream,
    status: tokio::sync::watch::Receiver<AccountSubscriptionStatus>,
    catalog: EnsuredRoute,
}

impl OrderStream {
    pub(crate) fn from_handle(handle: SubscriptionHandle, catalog: EnsuredRoute) -> Self {
        let status = map_status(handle.observe_status());
        Self {
            inner: handle.into_stream(),
            status,
            catalog,
        }
    }

    pub fn observe_status(&self) -> tokio::sync::watch::Receiver<AccountSubscriptionStatus> {
        self.status.clone()
    }
    pub fn status(&self) -> AccountSubscriptionStatus {
        self.status.borrow().clone()
    }
}

impl PositionStream {
    pub(crate) fn from_handle(handle: SubscriptionHandle, catalog: EnsuredRoute) -> Self {
        let status = map_status(handle.observe_status());
        Self {
            inner: handle.into_stream(),
            status,
            catalog,
        }
    }

    pub fn observe_status(&self) -> tokio::sync::watch::Receiver<AccountSubscriptionStatus> {
        self.status.clone()
    }
    pub fn status(&self) -> AccountSubscriptionStatus {
        self.status.borrow().clone()
    }
}

fn map_status(
    mut status: tokio::sync::watch::Receiver<crate::adapter::SubscriptionStatus>,
) -> tokio::sync::watch::Receiver<AccountSubscriptionStatus> {
    let (sender, receiver) = tokio::sync::watch::channel(status.borrow().clone().into());
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move {
            while status.changed().await.is_ok() {
                sender.send_replace(status.borrow().clone().into());
            }
        });
    }
    receiver
}

impl Stream for OrderStream {
    type Item = Result<Order, AccountError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(ProtocolEvent::Order(value)))) => {
                    let result = self
                        .catalog
                        .resolve_listing(&value.listing)
                        .map_err(account_error_from_catalog)
                        .and_then(|resolved| super::mapping::map_order(&value, &resolved));
                    return Poll::Ready(Some(result));
                }
                Poll::Ready(Some(Ok(_))) => continue,
                Poll::Ready(Some(Err(error))) => {
                    return Poll::Ready(Some(Err(account_error_from_market_participant(error))));
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl Stream for PositionStream {
    type Item = Result<Position, AccountError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(ProtocolEvent::Position(value)))) => {
                    let result = self
                        .catalog
                        .resolve_listing(&value.listing)
                        .map_err(account_error_from_catalog)
                        .and_then(|resolved| super::mapping::map_position(&value, &resolved));
                    return Poll::Ready(Some(result));
                }
                Poll::Ready(Some(Ok(_))) => continue,
                Poll::Ready(Some(Err(error))) => {
                    return Poll::Ready(Some(Err(account_error_from_market_participant(error))));
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
