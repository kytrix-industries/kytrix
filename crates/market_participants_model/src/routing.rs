//! Route-level shared types: capabilities and identifier normalization.
//!
//! Route descriptors themselves belong to the market participants connector and the
//! persisted endpoint/auth configuration belongs to `app_config`; this module
//! only owns the closed enums both of them share.

use std::fmt;

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::{
    error::ModelError,
    instrument::InstrumentKind,
    listing::venue::{BinanceProduct, HyperliquidAssetProduct, OkxInstrumentType},
};

/// The closed identifier kinds used by identifier normalization rules.
///
/// The kind corresponds one to one with the typed listing identifier variants,
/// so a route can never declare one normalization rule for two different native
/// identifier families.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ListingIdentifierKind {
    #[serde(rename = "symbol")]
    Symbol,
    #[serde(rename = "binance_instrument")]
    BinanceInstrument,
    #[serde(rename = "okx_instrument")]
    OkxInstrument,
    #[serde(rename = "hyperliquid_asset")]
    HyperliquidAsset,
    #[serde(rename = "hyperliquid_outcome")]
    HyperliquidOutcome,
    #[serde(rename = "polymarket_token")]
    PolymarketToken,
    #[serde(rename = "equity_local_code")]
    EquityLocalCode,
    #[serde(rename = "future_series")]
    FutureSeries,
    #[serde(rename = "occ_symbol")]
    OccSymbol,
    #[serde(rename = "market_participants_series")]
    MarketParticipantSeries,
    #[serde(rename = "option_local_symbol")]
    OptionLocalSymbol,
}

impl ListingIdentifierKind {
    /// Returns the fixed canonical label of this identifier kind.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::BinanceInstrument => "binance_instrument",
            Self::OkxInstrument => "okx_instrument",
            Self::HyperliquidAsset => "hyperliquid_asset",
            Self::HyperliquidOutcome => "hyperliquid_outcome",
            Self::PolymarketToken => "polymarket_token",
            Self::EquityLocalCode => "equity_local_code",
            Self::FutureSeries => "future_series",
            Self::OccSymbol => "occ_symbol",
            Self::MarketParticipantSeries => "market_participants_series",
            Self::OptionLocalSymbol => "option_local_symbol",
        }
    }

    /// Returns `true` when this kind carries a symbol that normalization applies
    /// to at all.
    pub const fn is_symbol_based(self) -> bool {
        !matches!(self, Self::PolymarketToken)
    }

    /// Returns `true` when this kind lives inside a series struct rather than
    /// being the primary index of a listing.
    pub const fn is_series_internal(self) -> bool {
        matches!(
            self,
            Self::OccSymbol | Self::MarketParticipantSeries | Self::OptionLocalSymbol
        )
    }
}

impl fmt::Display for ListingIdentifierKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// A transport or account capability of one venue route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum CapabilityId {
    #[serde(rename = "market_catalog")]
    MarketCatalog,
    #[serde(rename = "candlesticks")]
    Candlesticks,
    #[serde(rename = "market_trades")]
    MarketTrades,
    #[serde(rename = "tickers")]
    Tickers,
    #[serde(rename = "order_book")]
    OrderBook,
    #[serde(rename = "open_interest")]
    OpenInterest,
    #[serde(rename = "long_short_account_ratio")]
    LongShortAccountRatio,
    #[serde(rename = "top_trader_account_ratio")]
    TopTraderAccountRatio,
    #[serde(rename = "top_trader_position_ratio")]
    TopTraderPositionRatio,
    #[serde(rename = "taker_volume")]
    TakerVolume,
    #[serde(rename = "force_orders")]
    ForceOrders,
    #[serde(rename = "authenticated_trading")]
    AuthenticatedTrading,
    #[serde(rename = "account_orders")]
    AccountOrders,
    #[serde(rename = "account_positions")]
    AccountPositions,
    #[serde(rename = "account_balances")]
    AccountBalances,
    #[serde(rename = "account_trades")]
    AccountTrades,
    #[serde(rename = "account_history")]
    AccountHistory,
    #[serde(rename = "wallet_monitor")]
    WalletMonitor,
}

impl CapabilityId {
    /// Returns the fixed canonical label of this capability.
    pub const fn label(self) -> &'static str {
        match self {
            Self::MarketCatalog => "market_catalog",
            Self::Candlesticks => "candlesticks",
            Self::MarketTrades => "market_trades",
            Self::Tickers => "tickers",
            Self::OrderBook => "order_book",
            Self::OpenInterest => "open_interest",
            Self::LongShortAccountRatio => "long_short_account_ratio",
            Self::TopTraderAccountRatio => "top_trader_account_ratio",
            Self::TopTraderPositionRatio => "top_trader_position_ratio",
            Self::TakerVolume => "taker_volume",
            Self::ForceOrders => "force_orders",
            Self::AuthenticatedTrading => "authenticated_trading",
            Self::AccountOrders => "account_orders",
            Self::AccountPositions => "account_positions",
            Self::AccountBalances => "account_balances",
            Self::AccountTrades => "account_trades",
            Self::AccountHistory => "account_history",
            Self::WalletMonitor => "wallet_monitor",
        }
    }
}

impl fmt::Display for CapabilityId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// How a route normalizes one native identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SymbolNormalization {
    #[serde(rename = "exact")]
    Exact,
    #[serde(rename = "ascii_case_insensitive")]
    AsciiCaseInsensitive,
}

impl SymbolNormalization {
    /// Returns the fixed canonical label of this mode.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::AsciiCaseInsensitive => "ascii_case_insensitive",
        }
    }

    /// Applies this mode to a native symbol value.
    ///
    /// `Exact` rejects surrounding whitespace and preserves case;
    /// `AsciiCaseInsensitive` performs ASCII uppercase folding only, never
    /// Unicode case folding.
    pub fn apply(self, value: &str) -> Result<SmolStr, ModelError> {
        match self {
            Self::Exact => {
                if value.trim() != value {
                    return Err(ModelError::InvalidNativeIdentifier {
                        symbol: SmolStr::new(value),
                    });
                }
                Ok(SmolStr::new(value))
            }
            Self::AsciiCaseInsensitive => Ok(SmolStr::new(value.trim().to_ascii_uppercase())),
        }
    }
}

/// The product granularity of an identifier normalization rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "product")]
pub enum IdentifierNormalizationProduct {
    #[serde(rename = "instrument")]
    Instrument(InstrumentKind),
    #[serde(rename = "binance")]
    Binance(BinanceProduct),
    #[serde(rename = "okx")]
    Okx(OkxInstrumentType),
    #[serde(rename = "hyperliquid")]
    Hyperliquid(HyperliquidAssetProduct),
    #[serde(rename = "hyperliquid_outcome")]
    HyperliquidOutcome,
}

/// One identifier normalization rule of a route descriptor.
///
/// Rules are declared at typed identifier and optional product granularity, so
/// two identifiers with the same display text can never be merged by a single
/// route-level uppercase rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct IdentifierNormalizationRule {
    #[serde(rename = "identifier")]
    pub identifier: ListingIdentifierKind,
    #[serde(rename = "product")]
    pub product: Option<IdentifierNormalizationProduct>,
    #[serde(rename = "mode")]
    pub mode: SymbolNormalization,
}

impl IdentifierNormalizationRule {
    /// Builds a normalization rule.
    pub fn new(
        identifier: ListingIdentifierKind,
        product: Option<IdentifierNormalizationProduct>,
        mode: SymbolNormalization,
    ) -> Self {
        Self {
            identifier,
            product,
            mode,
        }
    }
}

/// Validates a route descriptor's identifier normalization rules.
///
/// Rules are unique and sorted by `(identifier, product)`, and the declared
/// product must match the typed identifier family it applies to.
pub fn validate_normalization_rules(
    rules: &[IdentifierNormalizationRule],
) -> Result<(), ModelError> {
    if rules
        .windows(2)
        .any(|pair| (pair[0].identifier, pair[0].product) >= (pair[1].identifier, pair[1].product))
    {
        return Err(ModelError::UnsortedCollection {
            field: "identifier_normalization",
        });
    }
    for rule in rules {
        if let Some(product) = rule.product
            && !product_matches(rule.identifier, product)
        {
            return Err(ModelError::IdentifierNormalizationMismatch {
                kind: rule.identifier,
            });
        }
    }
    Ok(())
}

fn product_matches(
    identifier: ListingIdentifierKind,
    product: IdentifierNormalizationProduct,
) -> bool {
    match product {
        IdentifierNormalizationProduct::Hyperliquid(_) => {
            identifier == ListingIdentifierKind::HyperliquidAsset
        }
        IdentifierNormalizationProduct::HyperliquidOutcome => {
            identifier == ListingIdentifierKind::HyperliquidOutcome
        }
        IdentifierNormalizationProduct::Binance(_) => {
            identifier == ListingIdentifierKind::BinanceInstrument
        }
        IdentifierNormalizationProduct::Okx(_) => {
            identifier == ListingIdentifierKind::OkxInstrument
        }
        IdentifierNormalizationProduct::Instrument(_) => !identifier.is_series_internal(),
    }
}

/// Resolves the normalization mode of one typed identifier.
///
/// A missing rule is `Exact`. A rule whose product does not match the typed
/// identifier returns [`ModelError::IdentifierNormalizationMismatch`] instead of
/// silently falling back to a route-level default.
pub fn normalization_mode(
    rules: &[IdentifierNormalizationRule],
    identifier: ListingIdentifierKind,
    product: Option<IdentifierNormalizationProduct>,
) -> Result<SymbolNormalization, ModelError> {
    if let Some(product) = product
        && !product_matches(identifier, product)
    {
        return Err(ModelError::IdentifierNormalizationMismatch { kind: identifier });
    }
    if let Some(product) = product {
        let exact = rules
            .iter()
            .find(|rule| rule.identifier == identifier && rule.product == Some(product));
        if let Some(rule) = exact {
            return Ok(rule.mode);
        }
    }
    let route_wide = rules
        .iter()
        .position(|rule| rule.identifier == identifier && rule.product.is_none());
    match route_wide {
        Some(index) => Ok(rules[index].mode),
        None => Ok(SymbolNormalization::Exact),
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        error::ModelError,
        instrument::InstrumentKind,
        listing::venue::{BinanceProduct, HyperliquidAssetProduct},
        routing::{
            CapabilityId, IdentifierNormalizationProduct, IdentifierNormalizationRule,
            ListingIdentifierKind, SymbolNormalization, normalization_mode,
            validate_normalization_rules,
        },
    };

    #[test]
    fn capability_ids_should_round_trip_through_serde() {
        let capability = CapabilityId::TopTraderPositionRatio;
        let encoded = serde_json::to_string(&capability).expect("capability should serialize");

        assert_eq!(encoded, "\"top_trader_position_ratio\"");
        assert_eq!(
            serde_json::from_str::<CapabilityId>(&encoded).expect("capability should decode"),
            capability
        );
    }

    #[test]
    fn exact_normalization_should_reject_surrounding_whitespace() {
        assert_eq!(
            SymbolNormalization::Exact
                .apply("xyz:ABC")
                .expect("value should normalize"),
            "xyz:ABC"
        );
        assert!(SymbolNormalization::Exact.apply(" xyz:ABC").is_err());
    }

    #[test]
    fn ascii_normalization_should_fold_case_only() {
        assert_eq!(
            SymbolNormalization::AsciiCaseInsensitive
                .apply("btcusdt")
                .expect("value should normalize"),
            "BTCUSDT"
        );
        assert_eq!(
            SymbolNormalization::AsciiCaseInsensitive
                .apply("ß")
                .expect("value should normalize"),
            "ß",
            "ASCII folding must not fold non-ASCII characters"
        );
    }

    #[test]
    fn normalization_rules_should_reject_a_product_kind_mismatch() {
        let rules = vec![IdentifierNormalizationRule::new(
            ListingIdentifierKind::OccSymbol,
            Some(IdentifierNormalizationProduct::Hyperliquid(
                HyperliquidAssetProduct::Spot,
            )),
            SymbolNormalization::Exact,
        )];

        assert_eq!(
            validate_normalization_rules(&rules),
            Err(ModelError::IdentifierNormalizationMismatch {
                kind: ListingIdentifierKind::OccSymbol
            })
        );
    }

    #[test]
    fn normalization_rules_should_be_sorted_and_unique() {
        let rules = vec![
            IdentifierNormalizationRule::new(
                ListingIdentifierKind::BinanceInstrument,
                Some(IdentifierNormalizationProduct::Binance(
                    BinanceProduct::Spot,
                )),
                SymbolNormalization::AsciiCaseInsensitive,
            ),
            IdentifierNormalizationRule::new(
                ListingIdentifierKind::BinanceInstrument,
                Some(IdentifierNormalizationProduct::Binance(
                    BinanceProduct::Spot,
                )),
                SymbolNormalization::Exact,
            ),
        ];

        assert_eq!(
            validate_normalization_rules(&rules),
            Err(ModelError::UnsortedCollection {
                field: "identifier_normalization"
            })
        );
    }

    #[test]
    fn missing_rules_should_default_to_exact() {
        assert_eq!(
            normalization_mode(
                &[],
                ListingIdentifierKind::HyperliquidOutcome,
                Some(IdentifierNormalizationProduct::HyperliquidOutcome)
            )
            .expect("mode should resolve"),
            SymbolNormalization::Exact
        );
        assert_eq!(
            normalization_mode(&[], ListingIdentifierKind::PolymarketToken, None)
                .expect("mode should resolve"),
            SymbolNormalization::Exact
        );
    }

    #[test]
    fn instrument_scoped_rules_should_not_apply_to_series_internal_identifiers() {
        let rules = vec![IdentifierNormalizationRule::new(
            ListingIdentifierKind::OccSymbol,
            Some(IdentifierNormalizationProduct::Instrument(
                InstrumentKind::Option,
            )),
            SymbolNormalization::AsciiCaseInsensitive,
        )];

        assert_eq!(
            normalization_mode(
                &rules,
                ListingIdentifierKind::OccSymbol,
                Some(IdentifierNormalizationProduct::Instrument(
                    InstrumentKind::Option
                ))
            ),
            Err(ModelError::IdentifierNormalizationMismatch {
                kind: ListingIdentifierKind::OccSymbol
            })
        );
    }

    #[test]
    fn hyperliquid_asset_should_never_use_a_route_wide_upcase_rule() {
        let product_rules = vec![IdentifierNormalizationRule::new(
            ListingIdentifierKind::HyperliquidAsset,
            Some(IdentifierNormalizationProduct::Hyperliquid(
                HyperliquidAssetProduct::Perpetual,
            )),
            SymbolNormalization::Exact,
        )];

        assert_eq!(
            normalization_mode(
                &product_rules,
                ListingIdentifierKind::HyperliquidAsset,
                Some(IdentifierNormalizationProduct::Hyperliquid(
                    HyperliquidAssetProduct::Perpetual
                ))
            )
            .expect("mode should resolve"),
            SymbolNormalization::Exact
        );
        assert_eq!(
            normalization_mode(
                &product_rules,
                ListingIdentifierKind::HyperliquidAsset,
                Some(IdentifierNormalizationProduct::Hyperliquid(
                    HyperliquidAssetProduct::Spot
                ))
            )
            .expect("an undeclared product must fall back to exact"),
            SymbolNormalization::Exact,
            "HIP-3 coins must never inherit another product's normalization"
        );

        let route_wide_rules = vec![IdentifierNormalizationRule::new(
            ListingIdentifierKind::HyperliquidAsset,
            None,
            SymbolNormalization::AsciiCaseInsensitive,
        )];
        assert_eq!(
            normalization_mode(
                &route_wide_rules,
                ListingIdentifierKind::HyperliquidAsset,
                Some(IdentifierNormalizationProduct::Hyperliquid(
                    HyperliquidAssetProduct::Spot
                ))
            )
            .expect("mode should resolve"),
            SymbolNormalization::AsciiCaseInsensitive
        );
    }
}
