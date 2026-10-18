//! The tradable listing and its published snapshot.

use ahash::AHashSet;
use serde::{Deserialize, Deserializer, Serialize, de};
use smol_str::SmolStr;

use crate::{
    decimal::UnixMillis,
    error::{ListingPublicationError, ModelError, require_non_negative_time},
    identifier::NativeSymbol,
    instrument::Instrument,
    listing::{
        continuity_series::{FutureSeriesIdentifiers, OptionSeriesIdentifiers},
        identifiers::{
            EchoValidationStatus, HyperliquidListingIdentifiers, VenueListingIdentifiers,
        },
        route::{VenueListingKey, VenueRoute},
        rules::TradingRules,
    },
    routing::{ListingIdentifierKind, SymbolNormalization},
    settlement::InstrumentId,
};

/// One listing: the canonical instrument it implements plus its native identity.
///
/// `identifiers` is the authoritative protocol identity; `native_symbol` and
/// `aliases` are display and lookup aliases of the same listing and never carry
/// identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VenueListing {
    #[serde(rename = "key")]
    pub key: VenueListingKey,
    #[serde(rename = "instrument_id")]
    pub instrument_id: InstrumentId,
    #[serde(rename = "native_symbol")]
    pub native_symbol: Option<NativeSymbol>,
    #[serde(rename = "aliases")]
    pub aliases: Vec<NativeSymbol>,
    #[serde(rename = "identifiers")]
    pub identifiers: VenueListingIdentifiers,
}

impl VenueListing {
    /// Builds a listing from its identity and typed native identifiers.
    ///
    /// The constructor validates the typed identifiers; the alias uniqueness and
    /// the instrument agreement are checked by [`VenueListing::validate_for`] at
    /// publication time.
    pub fn new(
        key: VenueListingKey,
        instrument_id: InstrumentId,
        native_symbol: Option<NativeSymbol>,
        aliases: Vec<NativeSymbol>,
        identifiers: VenueListingIdentifiers,
    ) -> Result<Self, ModelError> {
        let listing = Self {
            key,
            instrument_id,
            native_symbol,
            aliases,
            identifiers,
        };
        listing.identifiers.validate(&listing.key.route)?;
        Ok(listing)
    }

    /// Validates this listing against the canonical instrument it references.
    pub fn validate_for(&self, instrument: &Instrument) -> Result<(), ModelError> {
        if &self.instrument_id != instrument.id() {
            return Err(ModelError::InstrumentTermsMismatch {
                field: "venue_listing.instrument_id",
            });
        }
        self.identifiers.validate(&self.key.route)?;
        self.identifiers
            .validate_for(&self.key.route, instrument.kind())?;
        let mut symbols: AHashSet<&str> = AHashSet::with_capacity(self.aliases.len() + 1);
        for symbol in self
            .native_symbol
            .iter()
            .chain(self.aliases.iter())
            .map(NativeSymbol::as_str)
        {
            if !symbols.insert(symbol) {
                return Err(ModelError::DuplicateCollectionEntry {
                    field: "venue_listing.aliases",
                });
            }
        }
        Ok(())
    }

    /// Returns the primary native symbol of this listing.
    pub fn symbol(&self) -> Option<&NativeSymbol> {
        self.native_symbol.as_ref()
    }

    /// Validates that this listing belongs to the fetched route.
    pub fn validate_route(&self, route: &VenueRoute) -> Result<(), ModelError> {
        if &self.key.route == route {
            return Ok(());
        }
        Err(ModelError::ListingRouteMismatch {
            venue: route.venue,
            scope: route.scope.clone(),
            listing: self.key.clone(),
        })
    }

    /// Validates that `snapshot` describes this listing key.
    pub fn validate_snapshot(&self, snapshot: &ListingSnapshot) -> Result<(), ModelError> {
        if snapshot.key == self.key {
            return Ok(());
        }
        Err(ListingPublicationError::SnapshotKeyMismatch {
            listing: self.key.clone(),
            snapshot: snapshot.key.clone(),
        }
        .into())
    }

    /// Validates every symbol, ticker, coin or local code echoed by the typed
    /// identifiers against the display symbols of this listing.
    ///
    /// The typed identifiers are the authoritative protocol identity and
    /// `native_symbol`/`aliases` are display aliases of the same listing, so an
    /// echoed value must be one of them under the route normalization of its own
    /// identifier kind. The returned status distinguishes the two outcomes a
    /// caller has to treat differently: an echoed value that contradicts the
    /// declared symbols is a publication error, while a listing whose family has
    /// no comparable typed field — or that declares no display symbol to compare
    /// against — is publishable but has not had its native symbol verified.
    ///
    /// The status describes only the relationship between the typed identifiers
    /// and the declared aliases. It never promotes a native symbol into a name: a
    /// symbol a listing echoes is an alias a reader may search by, and the economic
    /// name of the listing is its instrument's name.
    pub fn validate_echoed_identifiers(
        &self,
        mode_of: impl Fn(ListingIdentifierKind) -> SymbolNormalization,
    ) -> Result<EchoValidationStatus, ModelError> {
        let echoed = self.echoed_identifiers();
        if echoed.is_empty() {
            return Ok(EchoValidationStatus::NoComparableIdentifier);
        }
        let mut declared: AHashSet<SmolStr> = AHashSet::with_capacity(self.aliases.len() + 1);
        for symbol in self
            .native_symbol
            .iter()
            .chain(self.aliases.iter())
            .map(NativeSymbol::as_str)
        {
            declared.insert(mode_of(ListingIdentifierKind::Symbol).apply(symbol)?);
        }
        if declared.is_empty() {
            // A listing does not have to declare a native symbol: equity and
            // outcome listings are published from typed identifiers alone, and
            // equity tickers are typed metadata rather than route aliases. With
            // nothing on the native side there is no echo to verify, so the
            // listing is publishable but its native symbols stay unverified.
            return Ok(EchoValidationStatus::NoComparableIdentifier);
        }
        for (kind, value) in echoed {
            if !declared.contains(&mode_of(kind).apply(value)?) {
                return Ok(EchoValidationStatus::Mismatch {
                    kind,
                    value: SmolStr::new(value),
                });
            }
        }
        Ok(EchoValidationStatus::Verified)
    }

    /// Returns the typed identifier values this listing can echo.
    ///
    /// A family echoes every verified identifier its own variant carries, each
    /// under its own [`ListingIdentifierKind`], so the comparison of
    /// [`VenueListing::validate_echoed_identifiers`] runs at the granularity the
    /// route declared: a route that declares one family verifies that family's
    /// value and nothing else, and a route that declares none leaves every value
    /// unverified instead of treating the missing comparison as a success.
    ///
    /// Only a family whose typed identifier names a value at all has an echo: a
    /// Hyperliquid spot listing carries a numeric asset index and a Polymarket
    /// listing carries a CLOB token, neither of which is a displayable symbol, so
    /// those families have no comparable identifier and can never be verified.
    fn echoed_identifiers(&self) -> Vec<(ListingIdentifierKind, &str)> {
        match &self.identifiers {
            VenueListingIdentifiers::Equity(identifiers) => vec![
                (
                    ListingIdentifierKind::EquityLocalCode,
                    identifiers.local_code.as_str(),
                ),
                (ListingIdentifierKind::Symbol, identifiers.ticker.as_str()),
            ],
            VenueListingIdentifiers::Hyperliquid(HyperliquidListingIdentifiers::Perpetual {
                coin,
                ..
            }) => vec![(ListingIdentifierKind::HyperliquidAsset, coin.as_str())],
            VenueListingIdentifiers::Hyperliquid(HyperliquidListingIdentifiers::Outcome {
                coin: Some(coin),
                ..
            }) => vec![(ListingIdentifierKind::HyperliquidOutcome, coin.as_str())],
            VenueListingIdentifiers::FutureSeries(series) => future_series_echoes(series),
            VenueListingIdentifiers::OptionSeries(series) => option_series_echoes(series),
            VenueListingIdentifiers::Binance(identifiers) => identifiers
                .future_series
                .as_ref()
                .map(future_series_echoes)
                .or_else(|| identifiers.option_series.as_ref().map(option_series_echoes))
                .unwrap_or_default(),
            VenueListingIdentifiers::Okx(identifiers) => identifiers
                .future_series
                .as_ref()
                .map(future_series_echoes)
                .or_else(|| identifiers.option_series.as_ref().map(option_series_echoes))
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// Returns the typed protocol outcome label of this listing, when it declares
    /// one.
    ///
    /// A Polymarket listing states the label the protocol publishes for the leg it
    /// trades. The echo check compares that label with the registry label of the same leg;
    /// it is never a name of its own, and the comparison is not an echo of a
    /// native display symbol, so it is not part of
    /// [`VenueListing::validate_echoed_identifiers`].
    pub fn protocol_outcome_label(&self) -> Option<&str> {
        match &self.identifiers {
            VenueListingIdentifiers::Polymarket(identifiers) => {
                Some(identifiers.outcome_label.as_str())
            }
            _ => None,
        }
    }

    /// Validates this listing together with its route, instrument and snapshot.
    ///
    /// This is the model-layer publication gate: the route must be the fetched
    /// route, the snapshot must describe this listing key, the instrument must be
    /// the one the listing declares, the snapshot rules must hold for that
    /// instrument, and every echoed identifier must agree with the display
    /// symbols under the route normalization.
    ///
    /// Only a contradicted echo refuses publication: a listing whose typed
    /// identifier has no declared symbol to compare against stays publishable,
    /// because no declared symbol contradicts it.
    pub fn validate_publication(
        &self,
        route: &VenueRoute,
        instrument: &Instrument,
        snapshot: &ListingSnapshot,
        mode_of: impl Fn(ListingIdentifierKind) -> SymbolNormalization,
    ) -> Result<(), ModelError> {
        self.validate_route(route)?;
        self.validate_snapshot(snapshot)?;
        self.validate_for(instrument)?;
        snapshot.validate_for(instrument)?;
        match self.validate_echoed_identifiers(mode_of)? {
            EchoValidationStatus::Mismatch { kind, value } => {
                Err(ListingPublicationError::IdentifierEchoMismatch { kind, value }.into())
            }
            EchoValidationStatus::Verified | EchoValidationStatus::NoComparableIdentifier => Ok(()),
        }
    }

    /// Returns the normalized native lookup keys of this listing.
    ///
    /// Every symbol-bearing identifier is normalized with its own rule, so two
    /// listings can only collide when the route explicitly declared the same
    /// normalization for the same typed identifier.
    pub fn normalized_keys(
        &self,
        mode_of: impl Fn(ListingIdentifierKind) -> SymbolNormalization,
    ) -> Result<Vec<SmolStr>, ModelError> {
        let mut keys: Vec<SmolStr> = Vec::with_capacity(self.aliases.len() + 2);
        for symbol in self
            .native_symbol
            .iter()
            .chain(self.aliases.iter())
            .map(|symbol| symbol.as_str())
        {
            keys.push(mode_of(ListingIdentifierKind::Symbol).apply(symbol)?);
        }
        match &self.identifiers {
            VenueListingIdentifiers::Binance(_) => {
                if let Some(symbol) = &self.native_symbol {
                    keys.push(
                        mode_of(ListingIdentifierKind::BinanceInstrument).apply(symbol.as_str())?,
                    );
                }
            }
            VenueListingIdentifiers::Okx(_) => {
                if let Some(symbol) = &self.native_symbol {
                    keys.push(
                        mode_of(ListingIdentifierKind::OkxInstrument).apply(symbol.as_str())?,
                    );
                }
            }
            VenueListingIdentifiers::Equity(identifiers) => {
                keys.push(
                    mode_of(ListingIdentifierKind::EquityLocalCode)
                        .apply(identifiers.local_code.as_str())?,
                );
            }
            VenueListingIdentifiers::Hyperliquid(_)
            | VenueListingIdentifiers::Polymarket(_)
            | VenueListingIdentifiers::FutureSeries(_)
            | VenueListingIdentifiers::OptionSeries(_) => {}
        }
        keys.sort();
        keys.dedup();
        Ok(keys)
    }
}

/// Returns the echoed values of one future series, in the registry's own order.
///
/// Each verified identifier is echoed under its own [`ListingIdentifierKind`], so
/// a route that declares only one family of a series verifies that family's value
/// and leaves the others unverified.
fn future_series_echoes(series: &FutureSeriesIdentifiers) -> Vec<(ListingIdentifierKind, &str)> {
    let mut echoed: Vec<(ListingIdentifierKind, &str)> = Vec::with_capacity(3);
    if let Some(id) = &series.market_participant_series_id {
        echoed.push((ListingIdentifierKind::MarketParticipantSeries, id.as_str()));
    }
    if let Some(symbol) = &series.local_symbol {
        echoed.push((ListingIdentifierKind::Symbol, symbol.as_str()));
    }
    if let Some(code) = &series.contract_code {
        echoed.push((ListingIdentifierKind::FutureSeries, code.as_str()));
    }
    echoed
}

/// Returns the echoed values of one option series, in the registry's own order.
fn option_series_echoes(series: &OptionSeriesIdentifiers) -> Vec<(ListingIdentifierKind, &str)> {
    let mut echoed: Vec<(ListingIdentifierKind, &str)> = Vec::with_capacity(3);
    if let Some(symbol) = &series.occ_symbol {
        echoed.push((ListingIdentifierKind::OccSymbol, symbol.as_str()));
    }
    if let Some(id) = &series.market_participant_series_id {
        echoed.push((ListingIdentifierKind::MarketParticipantSeries, id.as_str()));
    }
    if let Some(symbol) = &series.local_symbol {
        echoed.push((ListingIdentifierKind::OptionLocalSymbol, symbol.as_str()));
    }
    echoed
}

#[derive(Deserialize)]
struct VenueListingWire {
    #[serde(rename = "key")]
    key: VenueListingKey,
    #[serde(rename = "instrument_id")]
    instrument_id: InstrumentId,
    #[serde(rename = "native_symbol")]
    native_symbol: Option<NativeSymbol>,
    #[serde(rename = "aliases")]
    aliases: Vec<NativeSymbol>,
    #[serde(rename = "identifiers")]
    identifiers: VenueListingIdentifiers,
}

impl<'de> Deserialize<'de> for VenueListing {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = VenueListingWire::deserialize(deserializer)?;
        Self::new(
            wire.key,
            wire.instrument_id,
            wire.native_symbol,
            wire.aliases,
            wire.identifiers,
        )
        .map_err(de::Error::custom)
    }
}

/// The lifecycle status of one listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ListingStatus {
    #[serde(rename = "scheduled")]
    Scheduled,
    #[serde(rename = "trading")]
    Trading,
    #[serde(rename = "halted")]
    Halted,
    #[serde(rename = "expired")]
    Expired,
    #[serde(rename = "settled")]
    Settled,
    #[serde(rename = "delisted")]
    Delisted,
}

impl ListingStatus {
    /// Returns the fixed canonical label of this status.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Trading => "trading",
            Self::Halted => "halted",
            Self::Expired => "expired",
            Self::Settled => "settled",
            Self::Delisted => "delisted",
        }
    }

    /// Returns `true` when the status allows new trading commands.
    pub const fn allows_new_trading(self) -> bool {
        matches!(self, Self::Trading)
    }

    /// Returns `true` when the status allows settlement-side commands such as
    /// delivery, exercise or outcome settlement.
    pub const fn allows_settlement(self) -> bool {
        matches!(self, Self::Trading | Self::Halted | Self::Expired)
    }

    /// Returns `true` when the status blocks every command that changes
    /// settlement state.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Settled | Self::Delisted)
    }
}

/// The current rules, status and availability window of one listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ListingSnapshot {
    #[serde(rename = "key")]
    pub key: VenueListingKey,
    #[serde(rename = "rules")]
    pub rules: TradingRules,
    #[serde(rename = "status")]
    pub status: ListingStatus,
    #[serde(rename = "available_from")]
    pub available_from: Option<UnixMillis>,
    #[serde(rename = "available_until")]
    pub available_until: Option<UnixMillis>,
}

impl ListingSnapshot {
    /// Builds and validates a snapshot at the non-persisted instant `as_of`.
    ///
    /// `Scheduled` must carry `available_from = Some(t)` with `t > as_of`; a
    /// missing lower bound never means "about to be enabled".
    pub fn new(
        key: VenueListingKey,
        rules: TradingRules,
        status: ListingStatus,
        available_from: Option<UnixMillis>,
        available_until: Option<UnixMillis>,
        as_of: UnixMillis,
    ) -> Result<Self, ModelError> {
        let snapshot = Self {
            key,
            rules,
            status,
            available_from,
            available_until,
        };
        snapshot.validate(as_of)?;
        Ok(snapshot)
    }

    /// Validates the availability window and the status/window agreement.
    pub fn validate(&self, as_of: UnixMillis) -> Result<(), ModelError> {
        if let (Some(from), Some(until)) = (self.available_from, self.available_until) {
            require_non_negative_time(from, "available_from")?;
            require_non_negative_time(until, "available_until")?;
            if from > until {
                return Err(ModelError::InvalidTimeOrder {
                    field: "available_from",
                    limit: "available_until",
                });
            }
        } else if let Some(bound) = self.available_from.or(self.available_until) {
            require_non_negative_time(bound, "available_window")?;
        }
        if self.status == ListingStatus::Scheduled
            && self.available_from.is_none_or(|from| from <= as_of)
        {
            return Err(ModelError::ScheduledListingWithoutAvailability);
        }
        Ok(())
    }

    /// Validates the snapshot rules against the listed instrument.
    pub fn validate_for(&self, instrument: &Instrument) -> Result<(), ModelError> {
        self.rules.validate_for(instrument)
    }

    /// Validates that this snapshot describes `listing`.
    pub fn validate_listing(&self, listing: &VenueListing) -> Result<(), ModelError> {
        listing.validate_snapshot(self)
    }

    /// Returns `true` when `instant` is inside the half-open availability window.
    pub fn is_available_at(&self, instant: UnixMillis) -> bool {
        self.available_from.is_none_or(|from| instant >= from)
            && self.available_until.is_none_or(|until| instant < until)
    }

    /// Returns `true` when new trading is allowed at `instant`.
    pub fn allows_new_trading_at(&self, instant: UnixMillis) -> bool {
        self.status.allows_new_trading() && self.is_available_at(instant)
    }
}

#[derive(Deserialize)]
struct ListingSnapshotWire {
    #[serde(rename = "key")]
    key: VenueListingKey,
    #[serde(rename = "rules")]
    rules: TradingRules,
    #[serde(rename = "status")]
    status: ListingStatus,
    #[serde(rename = "available_from")]
    available_from: Option<UnixMillis>,
    #[serde(rename = "available_until")]
    available_until: Option<UnixMillis>,
}

impl<'de> Deserialize<'de> for ListingSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ListingSnapshotWire::deserialize(deserializer)?;
        let snapshot = Self {
            key: wire.key,
            rules: wire.rules,
            status: wire.status,
            available_from: wire.available_from,
            available_until: wire.available_until,
        };
        if let (Some(from), Some(until)) = (snapshot.available_from, snapshot.available_until)
            && from > until
        {
            return Err(de::Error::custom(ModelError::InvalidTimeOrder {
                field: "available_from",
                limit: "available_until",
            }));
        }
        Ok(snapshot)
    }
}
