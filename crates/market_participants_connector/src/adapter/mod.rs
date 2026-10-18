//! Capability-oriented market-participant adapter interfaces.

pub(crate) mod binance;
pub(crate) mod context;
pub(crate) mod error;
pub(crate) mod hyperliquid;
pub(crate) mod identity;
pub(crate) mod longbridge;
pub(crate) mod okx;
pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod r#trait;

pub(crate) use context::{Authentication, ExecutionContext, VenueExecutionContext};
pub(crate) use identity::bind_execution_identity;
pub(crate) use request::{PreparedRequest, ProtocolRequest};
pub(crate) use response::{
    NativeCandlestick, NativeCatalogEntry, NativeForceOrder, NativeListing, NativeLongShortRatio,
    NativeOpenInterest, NativeOrderBook, NativeOrderBookUpdate, NativeTakerVolume, NativeTicker,
    NativeTrade, ProtocolEvent, ProtocolResponse, ProtocolStream, SubscriptionHandle,
    SubscriptionStatus,
};
pub(crate) use r#trait::{AdapterLimits, MarketParticipantAdapter};
