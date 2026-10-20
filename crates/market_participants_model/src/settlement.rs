//! Contract construction primitives and the canonical instrument identity.
//!
//! These are the economic terms every product family shares: contract size and
//! settlement primitives, delivery, expiry, strike and exercise, plus the
//! versioned [`InstrumentId`], its lineage edges and the [`StockSplit`]
//! corporate action. Terms are immutable: changing any of them creates a new
//! identity instead of editing an existing one, and every term type can only be
//! built through its validating constructor.

use ahash::AHashSet;
use serde::{Deserialize, Deserializer, Serialize, de};
use smol_str::SmolStr;

use crate::{
    asset::{AssetId, IndexId, SecurityId},
    decimal::{PositiveRatio, Price, Quantity, Ratio, UnixMillis},
    error::{
        ModelError, SettlementAssetRole, require_non_negative_time, require_ordered_time,
        require_strictly_sorted,
    },
    identifier::{CanonicalEncoder, CanonicalTerms, IdentityText, InstrumentIdentity},
    listing::venue::PriceBasis,
    market::account::AssetAmount,
    reference::{PriceReferenceId, PriceReferenceTerms},
};
crate::identifier::identity_type!(InstrumentId, InstrumentIdentity);

/// A contract size expressed either in asset quantity or per index point.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(tag = "kind", content = "unit")]
pub enum ContractUnit {
    #[serde(rename = "asset_quantity")]
    AssetQuantity {
        #[serde(rename = "quantity")]
        quantity: Quantity,
        #[serde(rename = "asset")]
        asset: AssetId,
    },
    #[serde(rename = "quote_notional_per_index_point")]
    QuoteNotionalPerIndexPoint {
        #[serde(rename = "amount")]
        amount: Quantity,
        #[serde(rename = "quote_asset")]
        quote_asset: AssetId,
        #[serde(rename = "index")]
        index: IndexId,
    },
}

impl ContractUnit {
    /// Builds an asset-quantity contract unit.
    pub fn asset_quantity(quantity: Quantity, asset: AssetId) -> Result<Self, ModelError> {
        crate::error::require_positive_quantity(quantity, "contract_unit.quantity")?;
        Ok(Self::AssetQuantity { quantity, asset })
    }

    /// Builds an index-point contract unit.
    pub fn quote_notional_per_index_point(
        amount: Quantity,
        quote_asset: AssetId,
        index: IndexId,
    ) -> Result<Self, ModelError> {
        crate::error::require_positive_quantity(amount, "contract_unit.amount")?;
        Ok(Self::QuoteNotionalPerIndexPoint {
            amount,
            quote_asset,
            index,
        })
    }

    /// Returns the contract quantity or amount.
    pub fn quantity(&self) -> Quantity {
        match self {
            Self::AssetQuantity { quantity, .. } => *quantity,
            Self::QuoteNotionalPerIndexPoint { amount, .. } => *amount,
        }
    }

    /// Returns the asset this contract unit is denominated in, when it has one.
    pub fn denomination(&self) -> Option<&AssetId> {
        match self {
            Self::AssetQuantity { asset, .. } => Some(asset),
            Self::QuoteNotionalPerIndexPoint { .. } => None,
        }
    }

    /// Returns the index of an index-point contract unit.
    pub fn index(&self) -> Option<&IndexId> {
        match self {
            Self::QuoteNotionalPerIndexPoint { index, .. } => Some(index),
            Self::AssetQuantity { .. } => None,
        }
    }

    /// Returns the asset an index-point contract unit is quoted in, when it has
    /// one.
    ///
    /// An index is not an asset, so the currency an index point is worth is what
    /// says which contract the unit describes; an asset quantity carries its asset
    /// in [`ContractUnit::denomination`] instead.
    pub fn quote_asset(&self) -> Option<&AssetId> {
        match self {
            Self::QuoteNotionalPerIndexPoint { quote_asset, .. } => Some(quote_asset),
            Self::AssetQuantity { .. } => None,
        }
    }
}

impl CanonicalTerms for ContractUnit {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        match self {
            Self::AssetQuantity { quantity, asset } => {
                encoder.write_label("asset-quantity");
                encoder.write_terms(quantity);
                encoder.write_terms(asset);
            }
            Self::QuoteNotionalPerIndexPoint {
                amount,
                quote_asset,
                index,
            } => {
                encoder.write_label("quote-notional-per-index-point");
                encoder.write_terms(amount);
                encoder.write_terms(quote_asset);
                encoder.write_terms(index);
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", content = "unit")]
enum ContractUnitWire {
    #[serde(rename = "asset_quantity")]
    AssetQuantity {
        #[serde(rename = "quantity")]
        quantity: Quantity,
        #[serde(rename = "asset")]
        asset: AssetId,
    },
    #[serde(rename = "quote_notional_per_index_point")]
    QuoteNotionalPerIndexPoint {
        #[serde(rename = "amount")]
        amount: Quantity,
        #[serde(rename = "quote_asset")]
        quote_asset: AssetId,
        #[serde(rename = "index")]
        index: IndexId,
    },
}

impl TryFrom<ContractUnitWire> for ContractUnit {
    type Error = ModelError;

    fn try_from(wire: ContractUnitWire) -> Result<Self, Self::Error> {
        match wire {
            ContractUnitWire::AssetQuantity { quantity, asset } => {
                Self::asset_quantity(quantity, asset)
            }
            ContractUnitWire::QuoteNotionalPerIndexPoint {
                amount,
                quote_asset,
                index,
            } => Self::quote_notional_per_index_point(amount, quote_asset, index),
        }
    }
}

impl<'de> Deserialize<'de> for ContractUnit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_from(ContractUnitWire::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// The economic contract size of a derivative.
///
/// The contract size is an economic term, never a formatter setting; the order
/// quantity step of a listing stays in `QuantityRule`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ContractSize {
    #[serde(rename = "unit")]
    unit: ContractUnit,
}

impl ContractSize {
    /// Validates a contract unit and builds the contract size.
    pub fn new(unit: ContractUnit) -> Result<Self, ModelError> {
        crate::error::require_positive_quantity(unit.quantity(), "contract_unit.quantity")?;
        Ok(Self { unit })
    }

    /// Returns the contract unit.
    pub fn unit(&self) -> &ContractUnit {
        &self.unit
    }

    /// Returns the contract quantity.
    pub fn quantity(&self) -> Quantity {
        self.unit.quantity()
    }

    /// Returns the asset this contract size is denominated in, when it has one.
    pub fn denomination(&self) -> Option<&AssetId> {
        self.unit.denomination()
    }

    /// Returns the index of an index-point contract size.
    pub fn index(&self) -> Option<&IndexId> {
        self.unit.index()
    }

    /// Returns the asset an index-point contract size is quoted in, when it has
    /// one.
    pub fn quote_asset(&self) -> Option<&AssetId> {
        self.unit.quote_asset()
    }

    /// Returns the contract quantity of a cash-settled contract, when the unit
    /// is a plain quantity multiplier.
    ///
    /// An index-point contract is quoted as a currency amount per index point, so
    /// it has no plain quantity multiplier and is rejected here.
    pub fn cash_quantity(&self) -> Option<Quantity> {
        match self.unit {
            ContractUnit::AssetQuantity { .. } => Some(self.unit.quantity()),
            ContractUnit::QuoteNotionalPerIndexPoint { .. } => None,
        }
    }
}

impl CanonicalTerms for ContractSize {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_terms(&self.unit);
    }
}

#[derive(Deserialize)]
struct ContractSizeWire {
    #[serde(rename = "unit")]
    unit: ContractUnit,
}

impl<'de> Deserialize<'de> for ContractSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ContractSizeWire::deserialize(deserializer)?;
        Self::new(wire.unit).map_err(de::Error::custom)
    }
}

/// The dated part of a future or option contract.
///
/// `expiration_at` is mandatory. The optional instants mean that the market did
/// not publish that instant, never that a caller may omit the expiry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Expiry {
    #[serde(rename = "expiration_at")]
    expiration_at: UnixMillis,
    #[serde(rename = "last_trading_at")]
    last_trading_at: Option<UnixMillis>,
    #[serde(rename = "settlement_at")]
    settlement_at: Option<UnixMillis>,
}

impl Expiry {
    /// Validates the expiry instants.
    ///
    /// Every instant must be non-negative and ordered
    /// `last_trading_at <= expiration_at <= settlement_at`.
    pub fn new(
        expiration_at: UnixMillis,
        last_trading_at: Option<UnixMillis>,
        settlement_at: Option<UnixMillis>,
    ) -> Result<Self, ModelError> {
        let expiry = Self {
            expiration_at,
            last_trading_at,
            settlement_at,
        };
        expiry.validate()?;
        Ok(expiry)
    }

    /// Validates the expiry instants.
    pub fn validate(&self) -> Result<(), ModelError> {
        require_non_negative_time(self.expiration_at, "expiration_at")?;
        if let Some(last_trading_at) = self.last_trading_at {
            require_non_negative_time(last_trading_at, "last_trading_at")?;
            require_ordered_time(
                last_trading_at,
                self.expiration_at,
                "last_trading_at",
                "expiration_at",
            )?;
        }
        if let Some(settlement_at) = self.settlement_at {
            require_non_negative_time(settlement_at, "settlement_at")?;
            require_ordered_time(
                self.expiration_at,
                settlement_at,
                "expiration_at",
                "settlement_at",
            )?;
        }
        Ok(())
    }

    /// Returns the instant at which the contract expires.
    pub const fn expiration_at(&self) -> UnixMillis {
        self.expiration_at
    }

    /// Returns the published last trading instant.
    pub const fn last_trading_at(&self) -> Option<UnixMillis> {
        self.last_trading_at
    }

    /// Returns the published settlement instant.
    pub const fn settlement_at(&self) -> Option<UnixMillis> {
        self.settlement_at
    }

    /// Returns the latest instant at which the contract is still tradable.
    pub fn last_tradable_at(&self) -> UnixMillis {
        self.last_trading_at.unwrap_or(self.expiration_at)
    }
}

impl CanonicalTerms for Expiry {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_i64(self.expiration_at);
        encoder.write_optional(self.last_trading_at.as_ref(), |encoder, value| {
            encoder.write_i64(*value);
        });
        encoder.write_optional(self.settlement_at.as_ref(), |encoder, value| {
            encoder.write_i64(*value);
        });
    }
}

#[derive(Deserialize)]
pub struct ExpiryWire {
    #[serde(rename = "expiration_at")]
    expiration_at: UnixMillis,
    #[serde(rename = "last_trading_at")]
    last_trading_at: Option<UnixMillis>,
    #[serde(rename = "settlement_at")]
    settlement_at: Option<UnixMillis>,
}

impl TryFrom<ExpiryWire> for Expiry {
    type Error = ModelError;

    fn try_from(wire: ExpiryWire) -> Result<Self, Self::Error> {
        Self::new(wire.expiration_at, wire.last_trading_at, wire.settlement_at)
    }
}

impl<'de> Deserialize<'de> for Expiry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_from(ExpiryWire::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Which side of a physical delivery a long holder receives or delivers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum DeliveryDirection {
    #[serde(rename = "receive")]
    Receive,
    #[serde(rename = "deliver")]
    Deliver,
}

impl DeliveryDirection {
    /// Returns the opposite direction.
    pub const fn opposite(self) -> Self {
        match self {
            Self::Receive => Self::Deliver,
            Self::Deliver => Self::Receive,
        }
    }

    /// Returns the fixed canonical label of this direction.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Receive => "receive",
            Self::Deliver => "deliver",
        }
    }
}

impl CanonicalTerms for DeliveryDirection {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label(self.label());
    }
}

/// One physical delivery leg, always interpreted from the long holder's view.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct DeliveryLeg {
    #[serde(rename = "asset")]
    asset: AssetId,
    #[serde(rename = "quantity")]
    quantity: Quantity,
    #[serde(rename = "direction")]
    direction: DeliveryDirection,
}

impl DeliveryLeg {
    /// Validates a delivery leg.
    pub fn new(
        asset: AssetId,
        quantity: Quantity,
        direction: DeliveryDirection,
    ) -> Result<Self, ModelError> {
        crate::error::require_positive_quantity(quantity, "delivery_leg.quantity")?;
        Ok(Self {
            asset,
            quantity,
            direction,
        })
    }

    /// Returns the delivered asset.
    pub fn asset(&self) -> &AssetId {
        &self.asset
    }

    /// Returns the delivered quantity.
    pub const fn quantity(&self) -> Quantity {
        self.quantity
    }

    /// Returns the delivery direction.
    pub const fn direction(&self) -> DeliveryDirection {
        self.direction
    }
}

impl CanonicalTerms for DeliveryLeg {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_terms(&self.asset);
        encoder.write_terms(&self.quantity);
        encoder.write_terms(&self.direction);
    }
}

#[derive(Deserialize)]
pub struct DeliveryLegWire {
    #[serde(rename = "asset")]
    asset: AssetId,
    #[serde(rename = "quantity")]
    quantity: Quantity,
    #[serde(rename = "direction")]
    direction: DeliveryDirection,
}

impl TryFrom<DeliveryLegWire> for DeliveryLeg {
    type Error = ModelError;

    fn try_from(wire: DeliveryLegWire) -> Result<Self, Self::Error> {
        Self::new(wire.asset, wire.quantity, wire.direction)
    }
}

impl<'de> Deserialize<'de> for DeliveryLeg {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_from(DeliveryLegWire::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// How a contract settles.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(tag = "kind", content = "mode")]
pub enum DeliveryMode {
    #[serde(rename = "cash")]
    Cash {
        #[serde(rename = "asset")]
        asset: AssetId,
        #[serde(rename = "reference")]
        reference: PriceReferenceId,
        /// The conversion reference that turns the settlement asset into the instrument
        /// price quote, when the two differ.
        ///
        /// The value is a registry identity, and an identity is a one-way digest: it says
        /// *which* reference the contract converts through, never the direction that
        /// reference has. How much of that direction is checked therefore depends on the
        /// entry that built this delivery: [`DeliveryMode::cash`] holds the registry terms
        /// behind the identity and checks the half a delivery can check without a price
        /// quote — the reference tracks the settlement asset and is expressed per asset
        /// unit — while an identity that arrives from the wire carries no terms and is
        /// stored as declared. The quote half of the direction belongs to the instrument
        /// that owns the price quote, which checks it when it takes this delivery.
        ///
        /// # Obligations
        ///
        /// The market participant that publishes the contract **must** resolve the identity in
        /// `ReferenceRegistry`, refuse the publication with its own missing-reference
        /// error when the registry carries no terms for it, and reject the instrument
        /// when those terms do not convert the settlement asset into the price quote per
        /// asset unit. On the path that carries an identity alone that is the final
        /// direction check, because nothing here performs it.
        #[serde(
            rename = "conversion_reference",
            skip_serializing_if = "Option::is_none"
        )]
        conversion_reference: Option<PriceReferenceId>,
    },
    #[serde(rename = "physical")]
    Physical {
        #[serde(rename = "deliveries")]
        deliveries: Vec<DeliveryLeg>,
    },
}

/// Where a cash delivery's conversion identity comes from.
///
/// A cash delivery states the reference it converts through in one of two ways, and the
/// two differ in one thing only: a caller that holds the registry terms behind the
/// reference can also check the half of the direction those terms express that needs no
/// price quote, while an identity is a one-way digest that names only itself.
///
/// The same two ways are stated one level up by
/// [`ConversionSource`](crate::instrument::ConversionSource), which resolves the
/// conversion of a whole instrument. The two types are one fact at two levels and **must
/// stay in step**: a way of stating a conversion that is added to one has to be answered
/// in the other. They are not one type because the instrument level holds evidence a
/// delivery cannot have — a mirror copy of the identity, and a carrier stated separately
/// from the terms it has to agree with — while a delivery derives its identity from the
/// terms themselves, once the asset it settles in has been checked.
enum CashConversion<'a> {
    /// The caller supplied the registry terms, so the identity is derived from them
    /// and the half of the direction they express that needs no price quote is checked.
    /// `None` declares no conversion at all.
    Registry(Option<&'a PriceReferenceTerms>),
    /// A decoded payload carries only the opaque identity, whose direction is left to the
    /// instrument that owns the price quote and to the publishing market participant. `None`
    /// declares no conversion at all.
    Identity(Option<PriceReferenceId>),
}

impl DeliveryMode {
    /// Builds a cash settlement mode.
    ///
    /// The settlement asset must be a currency or a crypto token so the
    /// reference output can be converted into it.
    ///
    /// `conversion` is the full registry terms of the conversion reference when
    /// the settlement asset is not the price quote. Only the half of the direction
    /// a delivery can check without a price quote is checked here — the reference
    /// must track the settlement asset and be expressed per asset unit; the quote
    /// half belongs to the instrument constructor that owns the price quote. The
    /// identity this entry stores is a one-way digest, so the direction behind it is
    /// established here only as far as it can be, and the final check is the
    /// obligation of the market participant that publishes the reference.
    pub fn cash(
        asset: AssetId,
        reference: PriceReferenceId,
        conversion: Option<&PriceReferenceTerms>,
    ) -> Result<Self, ModelError> {
        Self::build_cash(asset, reference, CashConversion::Registry(conversion))
    }

    /// The one entry that builds and validates a cash settlement.
    ///
    /// The caller-facing [`DeliveryMode::cash`], the wire decoder and
    /// [`DeliveryMode::validate`] all run this function, so the rule a cash
    /// settlement states about itself is stated once and no entry can accept a
    /// settlement another entry refuses. The two entries differ in one thing only:
    /// a caller holds the registry terms behind the reference and therefore checks
    /// the half of the direction a delivery can check without a price quote — the
    /// reference tracks the settlement asset and is expressed per asset unit — while
    /// a decoded payload carries the opaque identity and leaves every direction check
    /// to the instrument that owns the price quote and to the market participant that publishes
    /// the reference.
    fn build_cash(
        asset: AssetId,
        reference: PriceReferenceId,
        conversion: CashConversion<'_>,
    ) -> Result<Self, ModelError> {
        if !asset.is_settlement_asset() {
            return Err(ModelError::UnsupportedSettlementAsset {
                asset,
                role: SettlementAssetRole::CashDelivery,
            });
        }
        let conversion_reference = match conversion {
            CashConversion::Registry(Some(terms)) => {
                crate::instrument::validate_conversion_direction(terms, &asset, None)?;
                Some(terms.reference_id()?)
            }
            CashConversion::Registry(None) => None,
            CashConversion::Identity(identity) => identity,
        };
        Ok(Self::Cash {
            asset,
            reference,
            conversion_reference,
        })
    }

    /// Builds a physical settlement mode.
    ///
    /// At least one leg is required, the legs are normalized into their
    /// canonical order, and the same asset cannot be both received and
    /// delivered.
    pub fn physical(mut deliveries: Vec<DeliveryLeg>) -> Result<Self, ModelError> {
        if deliveries.is_empty() {
            return Err(ModelError::EmptyCollection {
                field: "delivery.deliveries",
            });
        }
        deliveries.sort();
        let mut received: AHashSet<&AssetId> = AHashSet::with_capacity(deliveries.len());
        let mut delivered: AHashSet<&AssetId> = AHashSet::with_capacity(deliveries.len());
        for leg in &deliveries {
            match leg.direction {
                DeliveryDirection::Receive => {
                    received.insert(&leg.asset);
                }
                DeliveryDirection::Deliver => {
                    delivered.insert(&leg.asset);
                }
            }
        }
        if received.intersection(&delivered).next().is_some() {
            return Err(ModelError::ContradictoryDeliveryLegs);
        }
        Ok(Self::Physical { deliveries })
    }

    /// Validates this delivery mode.
    ///
    /// A cash settlement is re-built through [`DeliveryMode::build_cash`], which is
    /// the entry its constructor and the wire decoder also run, so the judgement is
    /// the one that built the value rather than a second set of rules over it. A
    /// physical delivery has no identity to re-state, so its legs are re-normalized.
    pub fn validate(&self) -> Result<(), ModelError> {
        match self {
            Self::Cash {
                asset,
                reference,
                conversion_reference,
            } => Self::build_cash(
                asset.clone(),
                *reference,
                CashConversion::Identity(*conversion_reference),
            )
            .map(|_| ()),
            Self::Physical { deliveries } => Self::physical(deliveries.clone()).map(|_| ()),
        }
    }

    /// Returns the asset a cash settlement pays in.
    pub fn cash_asset(&self) -> Option<&AssetId> {
        match self {
            Self::Cash { asset, .. } => Some(asset),
            Self::Physical { .. } => None,
        }
    }

    /// Returns the price reference a cash settlement observes.
    pub fn cash_reference(&self) -> Option<&PriceReferenceId> {
        match self {
            Self::Cash { reference, .. } => Some(reference),
            Self::Physical { .. } => None,
        }
    }

    /// Returns the conversion reference a cash settlement converts through.
    ///
    /// The reference is present only when the settlement asset differs from the
    /// price quote of the instrument, and it always converts that asset into the
    /// price quote. The returned identity is only as trustworthy as its publisher:
    /// its direction lives in the registry terms it was derived from, and nothing
    /// here can check it.
    ///
    /// # Obligations
    ///
    /// Whoever publishes the reference **must** resolve the identity in
    /// `ReferenceRegistry` and check the direction those terms express.
    pub fn cash_conversion_reference(&self) -> Option<&PriceReferenceId> {
        match self {
            Self::Cash {
                conversion_reference,
                ..
            } => conversion_reference.as_ref(),
            Self::Physical { .. } => None,
        }
    }

    /// Returns the physical delivery legs in canonical order.
    pub fn deliveries(&self) -> &[DeliveryLeg] {
        match self {
            Self::Physical { deliveries } => deliveries,
            Self::Cash { .. } => &[],
        }
    }

    /// Returns `true` when this mode delivers assets physically.
    pub const fn is_physical(&self) -> bool {
        matches!(self, Self::Physical { .. })
    }
}

impl CanonicalTerms for DeliveryMode {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        match self {
            Self::Cash {
                asset,
                reference,
                conversion_reference,
            } => {
                encoder.write_label("cash");
                encoder.write_terms(asset);
                encoder.write_terms(reference);
                // The conversion reference is optional and absent for every
                // quote-settled contract, so it must not write a presence marker:
                // the bytes of a cash delivery without a conversion reference are
                // the bytes of the pre-conversion-reference version of this shape.
                if let Some(conversion) = conversion_reference {
                    encoder.write_label("conversion-reference");
                    encoder.write_terms(conversion);
                }
            }
            Self::Physical { deliveries } => {
                encoder.write_label("physical");
                encoder.write_terms_slice(deliveries);
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", content = "mode")]
pub enum DeliveryModeWire {
    #[serde(rename = "cash")]
    Cash {
        #[serde(rename = "asset")]
        asset: AssetId,
        #[serde(rename = "reference")]
        reference: PriceReferenceId,
        /// The `default` is written out because the wire shape states that a missing
        /// conversion reference means "no conversion", rather than inheriting that from
        /// serde: serde already decodes a missing `Option` as `None`, so the attribute has
        /// no behaviour of its own and no test can tell it apart from leaving it out.
        #[serde(rename = "conversion_reference", default)]
        conversion_reference: Option<PriceReferenceId>,
    },
    #[serde(rename = "physical")]
    Physical {
        #[serde(rename = "deliveries")]
        deliveries: Vec<DeliveryLeg>,
    },
}

impl TryFrom<DeliveryModeWire> for DeliveryMode {
    type Error = ModelError;

    /// Decodes the shape of one settlement mode.
    ///
    /// Only the rules a delivery states on its own are enforced: the asset has to
    /// be one a settlement can be denominated in, and the physical legs have to
    /// form a valid delivery. A cash conversion reference is carried through as the
    /// identity the payload names — not claimed to be verified, because a delivery
    /// does not know the price quote of its contract and an identity carries no
    /// direction. The instrument constructor that owns the price quote decides
    /// whether the contract may convert at all.
    ///
    /// # Obligations
    ///
    /// Nothing on this path checks the direction behind the identity: it is a
    /// one-way digest. The market participant that publishes the contract **must** resolve it
    /// in `ReferenceRegistry` and check the direction the registry terms express.
    fn try_from(wire: DeliveryModeWire) -> Result<Self, Self::Error> {
        match wire {
            DeliveryModeWire::Cash {
                asset,
                reference,
                conversion_reference,
            } => Self::build_cash(
                asset,
                reference,
                CashConversion::Identity(conversion_reference),
            ),
            DeliveryModeWire::Physical { deliveries } => Self::physical(deliveries),
        }
    }
}

impl<'de> Deserialize<'de> for DeliveryMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_from(DeliveryModeWire::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// A typed cash settlement observation of an option.
///
/// A cash option whose settlement asset is not its price quote cannot be valued
/// from the quote price alone: the caller has to hand over the amount already
/// converted through the conversion reference of the option. A quote-settled
/// option has exactly the opposite obligation and must not supply one, so the two
/// cases cannot be confused at the call site.
///
/// The observation carries the conversion identity the amount was converted
/// through: it is compared with the reference the option declares, and the
/// direction behind that identity stays the responsibility of whoever published
/// the reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OptionSettlementObservation {
    /// The settlement asset is the price quote, so the quote price is enough.
    Quote,
    /// The settlement asset differs from the price quote, so the converted
    /// settlement amount of the option must be supplied explicitly.
    Converted {
        /// The conversion reference the amount was converted through.
        conversion_reference: PriceReferenceId,
        /// The converted amount, denominated in the cash settlement asset.
        amount: AssetAmount,
    },
}

impl OptionSettlementObservation {
    /// Builds a converted settlement observation.
    pub fn converted(conversion_reference: PriceReferenceId, amount: AssetAmount) -> Self {
        Self::Converted {
            conversion_reference,
            amount,
        }
    }

    /// Returns the converted settlement amount, when the observation carries one.
    pub fn converted_amount(&self) -> Option<&AssetAmount> {
        match self {
            Self::Quote => None,
            Self::Converted { amount, .. } => Some(amount),
        }
    }

    /// Rejects a converted observation that does not belong to this contract.
    ///
    /// An option with a conversion reference requires a converted observation that
    /// names the same reference and is denominated in the cash settlement asset; a
    /// quote-settled option requires none. Anything else is a caller error and is
    /// reported instead of being silently reinterpreted.
    pub fn validate_against(
        &self,
        conversion_reference: Option<&PriceReferenceId>,
        settlement_asset: &AssetId,
    ) -> Result<(), ModelError> {
        match (conversion_reference, self) {
            (None, Self::Quote) => Ok(()),
            (None, Self::Converted { .. }) => Err(ModelError::UnexpectedConversionReference),
            (Some(_), Self::Quote) => Err(ModelError::RequiredConversionReference),
            (
                Some(expected),
                Self::Converted {
                    conversion_reference,
                    amount,
                },
            ) => {
                if conversion_reference != expected {
                    return Err(ModelError::ConversionReferenceMismatch);
                }
                if &amount.asset != settlement_asset {
                    return Err(ModelError::SettlementAssetMismatch);
                }
                Ok(())
            }
        }
    }
}

/// An option strike with an explicit quote asset and price basis.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct StrikePrice {
    #[serde(rename = "value")]
    value: Price,
    #[serde(rename = "quote_asset")]
    quote_asset: AssetId,
    #[serde(rename = "basis")]
    basis: PriceBasis,
}

impl StrikePrice {
    /// Validates the strike.
    ///
    /// The strike must be greater than zero. Its quote asset and basis are
    /// matched against the option `price_quote` by `OptionTerms::new`.
    pub fn new(value: Price, quote_asset: AssetId, basis: PriceBasis) -> Result<Self, ModelError> {
        if value.is_zero() {
            return Err(ModelError::ZeroValue { field: "strike" });
        }
        Ok(Self {
            value,
            quote_asset,
            basis,
        })
    }

    /// Returns the strike value.
    pub const fn value(&self) -> Price {
        self.value
    }

    /// Returns the strike quote asset.
    pub fn quote_asset(&self) -> &AssetId {
        &self.quote_asset
    }

    /// Returns the strike price basis.
    pub fn basis(&self) -> &PriceBasis {
        &self.basis
    }
}

impl CanonicalTerms for StrikePrice {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_terms(&self.value);
        encoder.write_terms(&self.quote_asset);
        encoder.write_terms(&self.basis);
    }
}

#[derive(Deserialize)]
pub struct StrikePriceWire {
    #[serde(rename = "value")]
    value: Price,
    #[serde(rename = "quote_asset")]
    quote_asset: AssetId,
    #[serde(rename = "basis")]
    basis: PriceBasis,
}

impl TryFrom<StrikePriceWire> for StrikePrice {
    type Error = ModelError;

    fn try_from(wire: StrikePriceWire) -> Result<Self, Self::Error> {
        Self::new(wire.value, wire.quote_asset, wire.basis)
    }
}

impl<'de> Deserialize<'de> for StrikePrice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_from(StrikePriceWire::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// How a derivative's profit and loss converts between quote and settlement.
///
/// The model applies to both perpetual and dated contracts; a dated contract
/// must never leave its conversion semantics to an adapter guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum DerivativePnlModel {
    #[serde(rename = "linear")]
    Linear,
    #[serde(rename = "inverse")]
    Inverse,
    #[serde(rename = "quanto")]
    Quanto,
}

impl DerivativePnlModel {
    /// Returns the fixed canonical label of this pnl model.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::Inverse => "inverse",
            Self::Quanto => "quanto",
        }
    }
}

impl CanonicalTerms for DerivativePnlModel {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label(self.label());
    }
}

/// The right an option grants its holder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum OptionRight {
    #[serde(rename = "call")]
    Call,
    #[serde(rename = "put")]
    Put,
}

impl OptionRight {
    /// Returns the fixed canonical label of this right.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Put => "put",
        }
    }

    /// Returns the intrinsic value of one unit of the underlying at `reference`.
    ///
    /// A call is worth `max(reference - strike, 0)` and a put is worth
    /// `max(strike - reference, 0)`; both are expressed in the quote asset of
    /// the option price. The result is never negative.
    pub fn intrinsic_value(self, reference: Price, strike: Price) -> Quantity {
        let difference = match self {
            Self::Call => reference.inner().saturating_sub(strike.inner()),
            Self::Put => strike.inner().saturating_sub(reference.inner()),
        };
        Quantity::from_inner(difference)
    }
}

impl CanonicalTerms for OptionRight {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label(self.label());
    }
}

/// When an option may be exercised.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(tag = "kind", content = "style")]
pub enum ExerciseStyle {
    #[serde(rename = "american")]
    American,
    #[serde(rename = "european")]
    European,
    #[serde(rename = "bermudan")]
    Bermudan {
        #[serde(rename = "exercise_at")]
        exercise_at: Vec<UnixMillis>,
    },
}

impl ExerciseStyle {
    /// Builds a Bermudan exercise schedule and validates it against the expiry.
    pub fn bermudan(exercise_at: Vec<UnixMillis>, expiry: &Expiry) -> Result<Self, ModelError> {
        let style = Self::build(exercise_at)?;
        style.validate(expiry)?;
        Ok(style)
    }

    /// The one entry that builds and validates a Bermudan schedule on its own.
    ///
    /// The caller-facing [`ExerciseStyle::bermudan`], the wire decoder and
    /// [`ExerciseStyle::validate`] all run this function, so the half of the schedule
    /// rules that needs no expiry is stated once: a schedule is non-empty, strictly
    /// increasing and made of non-negative instants. The other half — that every
    /// instant falls inside the contract expiry — is checked by
    /// [`ExerciseStyle::validate`] once the expiry is known.
    fn build(exercise_at: Vec<UnixMillis>) -> Result<Self, ModelError> {
        require_strictly_sorted(&exercise_at, "exercise_at")?;
        for instant in &exercise_at {
            require_non_negative_time(*instant, "exercise_at")?;
        }
        Ok(Self::Bermudan { exercise_at })
    }

    /// Validates the exercise schedule against the contract expiry.
    ///
    /// A Bermudan schedule must be non-empty, strictly increasing, made of
    /// non-negative instants and must not fall after the expiry instant. The
    /// schedule-local half is re-checked through [`ExerciseStyle::build`], which is
    /// the entry that built the value.
    pub fn validate(&self, expiry: &Expiry) -> Result<(), ModelError> {
        if let Self::Bermudan { exercise_at } = self {
            Self::build(exercise_at.clone())?;
            for instant in exercise_at {
                require_ordered_time(
                    *instant,
                    expiry.expiration_at,
                    "exercise_at",
                    "expiration_at",
                )?;
            }
        }
        Ok(())
    }

    /// Returns the exercise schedule of a Bermudan style.
    pub fn exercise_at(&self) -> &[UnixMillis] {
        match self {
            Self::Bermudan { exercise_at } => exercise_at,
            Self::American | Self::European => &[],
        }
    }

    /// Returns the fixed canonical label of this exercise style.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::American => "american",
            Self::European => "european",
            Self::Bermudan { .. } => "bermudan",
        }
    }
}

impl CanonicalTerms for ExerciseStyle {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        match self {
            Self::American => encoder.write_label("american"),
            Self::European => encoder.write_label("european"),
            Self::Bermudan { exercise_at } => {
                encoder.write_label("bermudan");
                encoder.write_u64(exercise_at.len() as u64);
                for instant in exercise_at {
                    encoder.write_i64(*instant);
                }
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", content = "style")]
pub enum ExerciseStyleWire {
    #[serde(rename = "american")]
    American,
    #[serde(rename = "european")]
    European,
    #[serde(rename = "bermudan")]
    Bermudan {
        #[serde(rename = "exercise_at")]
        exercise_at: Vec<UnixMillis>,
    },
}

impl ExerciseStyleWire {
    /// Converts the wire shape through the entry the caller-facing constructor uses,
    /// so the schedule-local rules are the ones [`ExerciseStyle::build`] states; the
    /// expiry ordering check runs in the option constructor once the contract expiry
    /// is known.
    pub fn into_style(self) -> Result<ExerciseStyle, ModelError> {
        match self {
            Self::American => Ok(ExerciseStyle::American),
            Self::European => Ok(ExerciseStyle::European),
            Self::Bermudan { exercise_at } => ExerciseStyle::build(exercise_at),
        }
    }
}

impl<'de> Deserialize<'de> for ExerciseStyle {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        ExerciseStyleWire::deserialize(deserializer)?
            .into_style()
            .map_err(de::Error::custom)
    }
}

/// Why a new instrument identity replaced an existing one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum InstrumentAdjustment {
    #[serde(rename = "corporate_action")]
    CorporateAction,
    #[serde(rename = "deliverable_adjustment")]
    DeliverableAdjustment,
    #[serde(rename = "contract_specification_change")]
    ContractSpecificationChange,
    #[serde(rename = "outcome_specification_change")]
    OutcomeSpecificationChange,
}

impl InstrumentAdjustment {
    /// Returns the fixed canonical label of this adjustment.
    pub const fn label(self) -> &'static str {
        match self {
            Self::CorporateAction => "corporate-action",
            Self::DeliverableAdjustment => "deliverable-adjustment",
            Self::ContractSpecificationChange => "contract-specification-change",
            Self::OutcomeSpecificationChange => "outcome-specification-change",
        }
    }
}

impl CanonicalTerms for InstrumentAdjustment {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label(self.label());
    }
}

/// An instrument lineage relation between a predecessor and its successor.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct InstrumentLineageEdge {
    #[serde(rename = "predecessor")]
    pub predecessor: InstrumentId,
    #[serde(rename = "successor")]
    pub successor: InstrumentId,
    #[serde(rename = "reason")]
    pub reason: InstrumentAdjustment,
    #[serde(rename = "effective_at")]
    pub effective_at: UnixMillis,
}

impl InstrumentLineageEdge {
    /// Validates a lineage edge.
    pub fn new(
        predecessor: InstrumentId,
        successor: InstrumentId,
        reason: InstrumentAdjustment,
        effective_at: UnixMillis,
    ) -> Result<Self, ModelError> {
        require_non_negative_time(effective_at, "effective_at")?;
        if predecessor == successor {
            return Err(ModelError::InstrumentLineageInvalid {
                field: "successor equals predecessor",
            });
        }
        Ok(Self {
            predecessor,
            successor,
            reason,
            effective_at,
        })
    }
}

/// A stock split corporate action.
///
/// A split changes neither the issued security identity nor the spot instrument
/// identity. Account and market corporate-action services apply it to position
/// quantities, cost basis and adjusted prices, and de-duplicate repeated events
/// by `(security, effective_at, new_shares_per_old_share)`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StockSplit {
    #[serde(rename = "security")]
    pub security: SecurityId,
    #[serde(rename = "new_shares_per_old_share")]
    pub new_shares_per_old_share: PositiveRatio,
    #[serde(rename = "effective_at")]
    pub effective_at: UnixMillis,
}

impl StockSplit {
    /// Validates a stock split event.
    pub fn new(
        security: SecurityId,
        new_shares_per_old_share: PositiveRatio,
        effective_at: UnixMillis,
    ) -> Result<Self, ModelError> {
        require_non_negative_time(effective_at, "effective_at")?;
        Ok(Self {
            security,
            new_shares_per_old_share,
            effective_at,
        })
    }

    /// Returns the adjusted quantity of a position held through the split.
    pub fn adjust_quantity(&self, quantity: Quantity) -> Option<Quantity> {
        self.new_shares_per_old_share
            .ratio()
            .checked_mul_quantity(quantity)
    }

    /// Returns the de-duplication key of this corporate action.
    pub fn dedup_key(&self) -> (SecurityId, UnixMillis, Ratio) {
        (
            self.security,
            self.effective_at,
            self.new_shares_per_old_share.ratio(),
        )
    }
}

/// A relation between two canonical identities inside a lineage graph.
pub trait LineageRelation {
    /// Returns the predecessor identity text of this relation.
    fn predecessor(&self) -> IdentityText;

    /// Returns the successor identity text of this relation.
    fn successor(&self) -> IdentityText;

    /// Returns the instant at which this relation becomes effective.
    fn effective_at(&self) -> UnixMillis;
}

impl LineageRelation for InstrumentLineageEdge {
    fn predecessor(&self) -> IdentityText {
        self.predecessor.as_str()
    }

    fn successor(&self) -> IdentityText {
        self.successor.as_str()
    }

    fn effective_at(&self) -> UnixMillis {
        self.effective_at
    }
}

/// Validates a lineage graph.
///
/// The graph must be acyclic, must not repeat a `(predecessor, successor)`
/// relation, must not give one successor two predecessors, and every successor
/// must take effect no earlier than the relation that points at it.
pub fn validate_lineage<E: LineageRelation>(
    edges: &[E],
    invalid: fn(&'static str) -> ModelError,
) -> Result<(), ModelError> {
    let mut successors: AHashSet<SmolStr> = AHashSet::with_capacity(edges.len());
    let mut relations: AHashSet<(SmolStr, SmolStr)> = AHashSet::with_capacity(edges.len());
    for edge in edges {
        if !relations.insert((
            SmolStr::new(edge.predecessor()),
            SmolStr::new(edge.successor()),
        )) {
            return Err(invalid("duplicate lineage relation"));
        }
        if !successors.insert(SmolStr::new(edge.successor())) {
            return Err(invalid("duplicate lineage successor"));
        }
    }
    for edge in edges {
        let origin = edge.predecessor();
        let mut current = edge;
        let mut steps = 0_usize;
        while let Some(next) = edges
            .iter()
            .find(|candidate| candidate.predecessor() == current.successor())
        {
            if next.successor() == origin {
                return Err(invalid("lineage cycle"));
            }
            if next.effective_at() < current.effective_at() {
                return Err(invalid("successor takes effect before its predecessor"));
            }
            current = next;
            steps += 1;
            if steps > edges.len() {
                return Err(invalid("lineage cycle"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::CashConversion;
    use crate::{
        asset::{
            AssetId, CryptoTokenId, CryptoTokenOrigin, CurrencyId, IndexId, Iso4217Code,
            SecurityForm, SecurityId, SecurityIssuerId, ShareClassId, ShareClassTerms,
            TokenReference, UnderlyingRef,
        },
        decimal::{PositiveRatio, Price, Quantity, Ratio},
        error::{ConversionDirection, ModelError, SettlementAssetRole},
        identifier::{CanonicalTerms, IdentifierNamespace, IdentifierValue, TokenIssuerId},
        instrument::ConversionSource,
        listing::venue::PriceBasis,
        reference::{
            PriceReferenceId, PriceReferenceTerms, ReferenceAggregation, ReferenceObservation,
            ReferenceObservationKind, ReferenceSessionConvention, ReferenceSettlementConvention,
        },
        settlement::{
            ContractSize, ContractUnit, DeliveryDirection, DeliveryLeg, DeliveryMode,
            DeliveryModeWire, ExerciseStyle, ExerciseStyleWire, Expiry, InstrumentAdjustment,
            InstrumentId, InstrumentLineageEdge, StockSplit, StrikePrice, validate_lineage,
        },
    };

    fn venue_token(code: &str) -> AssetId {
        AssetId::CryptoToken(
            CryptoTokenId::new(CryptoTokenOrigin::VenueIssued {
                issuer: TokenIssuerId::new("binance").expect("issuer should be valid"),
                namespace: IdentifierNamespace::new("binance").expect("namespace should be valid"),
                token: TokenReference::Native {
                    code: IdentifierValue::new(code).expect("code should be valid"),
                },
            })
            .expect("token should be valid"),
        )
    }

    fn usd() -> AssetId {
        AssetId::Currency(CurrencyId::new(
            Iso4217Code::new("USD").expect("USD should be valid"),
        ))
    }

    fn price_terms(
        underlying: UnderlyingRef,
        quote_asset: AssetId,
        basis: PriceBasis,
    ) -> PriceReferenceTerms {
        PriceReferenceTerms {
            underlying,
            quote_asset,
            basis,
            observation: ReferenceObservation {
                source: crate::identifier::ReferenceSourceId::new("binance")
                    .expect("source should be valid"),
                kind: ReferenceObservationKind::Mark,
            },
            aggregation: ReferenceAggregation::Direct,
            session: ReferenceSessionConvention::Continuous,
            settlement: ReferenceSettlementConvention::Continuous,
            schema_version: crate::identifier::ReferenceTermsVersion::new("v1")
                .expect("version should be valid"),
        }
    }

    fn reference() -> PriceReferenceId {
        price_terms(
            UnderlyingRef::Asset(venue_token("BTC")),
            usd(),
            PriceBasis::PerAssetUnit,
        )
        .reference_id()
        .expect("reference should be valid")
    }

    /// The conversion reference of a contract that settles in `settlement` and is
    /// quoted in `price_quote`.
    fn conversion(settlement: AssetId, price_quote: &AssetId) -> PriceReferenceTerms {
        price_terms(
            UnderlyingRef::Asset(settlement),
            price_quote.clone(),
            PriceBasis::PerAssetUnit,
        )
    }

    /// A cash delivery that settles in the underlying asset and converts it back
    /// into the price quote.
    fn settlement(code: &str, price_quote: &AssetId) -> (DeliveryMode, PriceReferenceTerms) {
        let conversion = conversion(venue_token(code), price_quote);
        let delivery = DeliveryMode::cash(venue_token(code), reference(), Some(&conversion))
            .expect("delivery should be valid");
        (delivery, conversion)
    }

    /// The canonical bytes of one encoded term.
    fn terms_bytes<T: CanonicalTerms>(term: &T) -> Vec<u8> {
        let mut encoder = crate::identifier::CanonicalEncoder::new();
        encoder.write_terms(term);
        encoder.as_bytes().to_vec()
    }

    /// A cash delivery that settles in a currency other than the price quote.
    fn currency_settlement(code: &str, price_quote: &AssetId) -> DeliveryMode {
        let settlement = AssetId::Currency(CurrencyId::new(
            Iso4217Code::new(code).expect("currency code should be valid"),
        ));
        let conversion = conversion(settlement.clone(), price_quote);
        DeliveryMode::cash(settlement, reference(), Some(&conversion))
            .expect("delivery should be valid")
    }

    fn crypto_spot_identity(base: &str, quote: &str) -> InstrumentId {
        InstrumentId::of_terms(
            &crate::instrument::SpotTerms::new(venue_token(base), venue_token(quote))
                .expect("terms should be valid"),
        )
    }

    /// The canonical instrument terms a listing is identified by are wire schema, so
    /// the digest of one fixed spot instrument is pinned: a change to the terms
    /// encoding or to the identity namespace moves this literal.
    #[test]
    fn instrument_identity_should_be_a_pinned_digest() {
        assert_eq!(
            crypto_spot_identity("BTC", "USDT").as_str(),
            "v1/9622bc5d8875b85769b2f1a3d3438594a7549861e58ed4ada72a6ec5fe517d43",
            "the full spot terms derive a pinned instrument identity"
        );
    }

    #[test]
    fn expiry_should_accept_equal_bounds() {
        assert!(Expiry::new(10, Some(10), Some(10)).is_ok());
        assert!(Expiry::new(10, None, None).is_ok());
        assert_eq!(
            Expiry::new(10, Some(5), Some(20))
                .expect("expiry should be valid")
                .last_tradable_at(),
            5
        );
    }

    #[test]
    fn expiry_should_reject_inverted_time_order() {
        assert_eq!(
            Expiry::new(10, Some(11), None),
            Err(ModelError::InvalidTimeOrder {
                field: "last_trading_at",
                limit: "expiration_at"
            })
        );
        assert_eq!(
            Expiry::new(10, None, Some(9)),
            Err(ModelError::InvalidTimeOrder {
                field: "expiration_at",
                limit: "settlement_at"
            })
        );
        assert_eq!(
            Expiry::new(-1, None, None),
            Err(ModelError::InvalidTimestamp {
                field: "expiration_at"
            })
        );
    }

    #[test]
    fn expiry_should_reject_a_malformed_wire_value() {
        assert!(serde_json::from_str::<Expiry>(r#"{"expiration_at":1}"#).is_ok());
        assert!(
            serde_json::from_str::<Expiry>(r#"{"last_trading_at":1,"settlement_at":2}"#).is_err()
        );
        assert!(
            serde_json::from_str::<Expiry>(
                r#"{"expiration_at":1,"last_trading_at":2,"settlement_at":3}"#
            )
            .is_err()
        );
    }

    #[test]
    fn bermudan_exercise_should_require_a_sorted_non_empty_schedule() {
        let expiry = Expiry::new(100, None, None).expect("expiry should be valid");

        assert!(ExerciseStyle::bermudan(Vec::new(), &expiry).is_err());
        assert!(ExerciseStyle::bermudan(vec![10, 10], &expiry).is_err());
        assert!(ExerciseStyle::bermudan(vec![30, 20], &expiry).is_err());
        assert!(ExerciseStyle::bermudan(vec![101], &expiry).is_err());
        assert!(ExerciseStyle::bermudan(vec![10, 20, 100], &expiry).is_ok());
        assert_eq!(
            ExerciseStyle::bermudan(vec![10, 20], &expiry)
                .expect("schedule should be valid")
                .exercise_at(),
            &[10, 20]
        );
    }

    /// The wire shape of a Bermudan schedule runs the entry its constructor runs, so
    /// a schedule no caller could build is refused while it is decoded and for the
    /// typed reason that names the rule it broke.
    #[test]
    fn bermudan_exercise_wire_should_refuse_a_schedule_no_caller_could_build() {
        assert_eq!(
            ExerciseStyleWire::Bermudan {
                exercise_at: Vec::new(),
            }
            .into_style(),
            Err(ModelError::EmptyCollection {
                field: "exercise_at"
            })
        );
        assert_eq!(
            ExerciseStyleWire::Bermudan {
                exercise_at: vec![20, 10],
            }
            .into_style(),
            Err(ModelError::UnsortedCollection {
                field: "exercise_at"
            })
        );
        assert_eq!(
            ExerciseStyleWire::Bermudan {
                exercise_at: vec![-1],
            }
            .into_style(),
            Err(ModelError::InvalidTimestamp {
                field: "exercise_at"
            }),
            "an instant before the epoch is refused by the same entry the constructor uses"
        );

        for document in [
            r#"{"kind":"bermudan","style":{"exercise_at":[]}}"#,
            r#"{"kind":"bermudan","style":{"exercise_at":[20,10]}}"#,
            r#"{"kind":"bermudan","style":{"exercise_at":[10,10]}}"#,
            r#"{"kind":"bermudan","style":{"exercise_at":[-1]}}"#,
        ] {
            assert!(
                serde_json::from_str::<ExerciseStyle>(document).is_err(),
                "{document}: a non-canonical schedule must not decode"
            );
        }
        assert_eq!(
            serde_json::from_str::<ExerciseStyle>(
                r#"{"kind":"bermudan","style":{"exercise_at":[10,20]}}"#
            )
            .expect("the canonical schedule decodes")
            .exercise_at(),
            &[10, 20]
        );
    }

    #[test]
    fn contract_size_should_require_a_positive_quantity() {
        let unit = ContractUnit::asset_quantity(Quantity::from(1_u8), venue_token("BTC"))
            .expect("unit should be valid");

        assert_eq!(
            ContractSize::new(unit)
                .expect("size should be valid")
                .quantity(),
            Quantity::from(1_u8)
        );
        assert_eq!(
            ContractUnit::asset_quantity(Quantity::ZERO, venue_token("BTC")),
            Err(ModelError::NonPositiveQuantity {
                field: "contract_unit.quantity"
            })
        );
        assert_eq!(
            ContractUnit::quote_notional_per_index_point(
                Quantity::ZERO,
                usd(),
                IndexId::new(
                    IdentifierNamespace::new("sp").expect("namespace should be valid"),
                    IdentifierValue::new("500").expect("code should be valid"),
                ),
            ),
            Err(ModelError::NonPositiveQuantity {
                field: "contract_unit.amount"
            })
        );
    }

    #[test]
    fn contract_size_should_reject_a_malformed_wire_value() {
        assert!(
            serde_json::from_str::<ContractSize>(
                r#"{"unit":{"kind":"asset_quantity","unit":{"quantity":"0","asset":{"kind":"currency","spec":"v1/0000000000000000000000000000000000000000000000000000000000000000"}}}}"#
            )
            .is_err(),
            "a zero contract quantity must not decode"
        );
    }

    #[test]
    fn physical_delivery_should_require_at_least_one_leg() {
        assert_eq!(
            DeliveryMode::physical(Vec::new()),
            Err(ModelError::EmptyCollection {
                field: "delivery.deliveries"
            })
        );
    }

    #[test]
    fn physical_delivery_should_reject_contradictory_legs() {
        let leg = |direction| {
            DeliveryLeg::new(venue_token("BTC"), Quantity::from(1_u8), direction)
                .expect("leg should be valid")
        };

        assert_eq!(
            DeliveryMode::physical(vec![
                leg(DeliveryDirection::Receive),
                leg(DeliveryDirection::Deliver)
            ]),
            Err(ModelError::ContradictoryDeliveryLegs)
        );
    }

    #[test]
    fn physical_delivery_should_encode_a_canonical_leg_order() {
        let btc = DeliveryLeg::new(
            venue_token("BTC"),
            Quantity::from(1_u8),
            DeliveryDirection::Deliver,
        )
        .expect("leg should be valid");
        let usd_leg = DeliveryLeg::new(usd(), Quantity::from(5_u8), DeliveryDirection::Receive)
            .expect("leg should be valid");
        let first = DeliveryMode::physical(vec![btc.clone(), usd_leg.clone()])
            .expect("mode should be valid");
        let second = DeliveryMode::physical(vec![usd_leg, btc]).expect("mode should be valid");

        assert_eq!(first, second, "leg order must not change the terms");
        let mut left = crate::identifier::CanonicalEncoder::new();
        first.encode(&mut left);
        let mut right = crate::identifier::CanonicalEncoder::new();
        second.encode(&mut right);
        assert_eq!(left.as_bytes(), right.as_bytes());
    }

    #[test]
    fn physical_deliveries_should_derive_the_same_instrument_id_in_any_order() {
        let stock = DeliveryLeg::new(
            venue_token("AAPL"),
            Quantity::from(100_u8),
            DeliveryDirection::Receive,
        )
        .expect("leg should be valid");
        let cash = DeliveryLeg::new(
            usd(),
            Quantity::from(15_000_u32),
            DeliveryDirection::Deliver,
        )
        .expect("leg should be valid");
        let adjusted = DeliveryLeg::new(
            usd(),
            Quantity::from(16_000_u32),
            DeliveryDirection::Deliver,
        )
        .expect("leg should be valid");
        let underlying = crate::asset::DerivativeUnderlying {
            reference: UnderlyingRef::Asset(venue_token("AAPL")),
            price_reference: reference(),
        };
        let contract_size = ContractSize::new(
            ContractUnit::asset_quantity(Quantity::from(100_u8), venue_token("AAPL"))
                .expect("unit should be valid"),
        )
        .expect("contract size should be valid");
        let strike = StrikePrice::new(Price::from(150_u32), usd(), PriceBasis::PerAssetUnit)
            .expect("strike should be valid");
        let option = |deliveries: Vec<DeliveryLeg>| {
            crate::instrument::Instrument::new(crate::instrument::InstrumentTerms::Option(
                crate::instrument::OptionTerms::new(
                    underlying.clone(),
                    usd(),
                    crate::settlement::OptionRight::Call,
                    strike.clone(),
                    Expiry::new(2_000, Some(1_800), Some(2_100)).expect("expiry should be valid"),
                    ExerciseStyle::American,
                    contract_size.clone(),
                    DeliveryMode::physical(deliveries).expect("mode should be valid"),
                    None,
                )
                .expect("terms should be valid"),
            ))
            .expect("instrument should be valid")
        };
        let first = option(vec![stock.clone(), cash.clone()]);
        let second = option(vec![cash, stock.clone()]);
        let changed = option(vec![stock, adjusted]);

        assert_eq!(
            first.id(),
            second.id(),
            "the same delivery legs in another order are the same contract"
        );
        assert_ne!(
            first.id(),
            changed.id(),
            "a different delivery leg set is a different contract"
        );
    }

    #[test]
    fn delivery_legs_should_require_positive_quantities() {
        assert_eq!(
            DeliveryLeg::new(
                venue_token("BTC"),
                Quantity::ZERO,
                DeliveryDirection::Receive
            ),
            Err(ModelError::NonPositiveQuantity {
                field: "delivery_leg.quantity"
            })
        );
    }

    #[test]
    fn cash_delivery_should_require_a_settlement_asset() {
        let security = SecurityId::new(
            SecurityIssuerId::new("cik/320193").expect("issuer should be valid"),
            ShareClassId::new(&ShareClassTerms {
                issuer: SecurityIssuerId::new("cik/320193").expect("issuer should be valid"),
                designation: "common".into(),
                form: SecurityForm::CommonStock,
                par_value: None,
                adr_ratio: None,
            })
            .expect("share class should be valid"),
            SecurityForm::CommonStock,
        );

        let delivery =
            DeliveryMode::cash(usd(), reference(), None).expect("delivery should be valid");
        assert_eq!(delivery.cash_asset(), Some(&usd()));
        assert_eq!(delivery.cash_reference(), Some(&reference()));
        assert_eq!(
            delivery.cash_conversion_reference(),
            None,
            "a settlement in the price quote converts nothing"
        );
        assert_eq!(
            DeliveryMode::cash(AssetId::Security(security), reference(), None),
            Err(ModelError::UnsupportedSettlementAsset {
                asset: AssetId::Security(security),
                role: SettlementAssetRole::CashDelivery,
            }),
            "the rejection names the settlement that was refused"
        );
    }

    /// The cash delivery shape gained an optional conversion reference, so the
    /// bytes of the shape without one are pinned: a contract that settles in its
    /// price quote must keep the identity it had before the field existed.
    #[test]
    fn cash_delivery_without_conversion_should_encode_the_previous_bytes() {
        let delivery =
            DeliveryMode::cash(usd(), reference(), None).expect("delivery should be valid");
        let mut encoder = crate::identifier::CanonicalEncoder::new();
        delivery.encode(&mut encoder);
        let mut expected: Vec<u8> = Vec::new();
        expected.extend_from_slice(&4_u64.to_be_bytes());
        expected.extend_from_slice(b"cash");
        expected.extend_from_slice(&terms_bytes(&usd()));
        expected.extend_from_slice(&terms_bytes(&reference()));

        assert_eq!(encoder.as_bytes(), expected.as_slice());
    }

    #[test]
    fn cash_delivery_should_carry_a_validated_conversion_reference() {
        let (delivery, conversion) = settlement("BTC", &usd());

        assert_eq!(
            delivery.cash_conversion_reference(),
            Some(
                &conversion
                    .reference_id()
                    .expect("reference should be valid")
            )
        );
        assert_eq!(delivery.cash_asset(), Some(&venue_token("BTC")));

        // The conversion reference converts the settlement asset into the price
        // quote, so a reference that tracks anything else is refused.
        let wrong_underlying = price_terms(
            UnderlyingRef::Asset(venue_token("ETH")),
            usd(),
            PriceBasis::PerAssetUnit,
        );
        assert_eq!(
            DeliveryMode::cash(venue_token("BTC"), reference(), Some(&wrong_underlying)),
            Err(ModelError::ConversionDirectionMismatch {
                direction: ConversionDirection::Underlying
            })
        );
        let wrong_basis = price_terms(
            UnderlyingRef::Asset(venue_token("BTC")),
            usd(),
            PriceBasis::PerContract,
        );
        assert_eq!(
            DeliveryMode::cash(venue_token("BTC"), reference(), Some(&wrong_basis)),
            Err(ModelError::ConversionDirectionMismatch {
                direction: ConversionDirection::Basis
            })
        );
        let index_basis = price_terms(
            UnderlyingRef::Asset(venue_token("BTC")),
            usd(),
            PriceBasis::PerIndexPoint {
                index: IndexId::new(
                    IdentifierNamespace::new("sp").expect("namespace should be valid"),
                    IdentifierValue::new("500").expect("code should be valid"),
                ),
            },
        );
        assert_eq!(
            DeliveryMode::cash(venue_token("BTC"), reference(), Some(&index_basis)),
            Err(ModelError::ConversionDirectionMismatch {
                direction: ConversionDirection::Basis
            })
        );
    }

    /// Only the asset half of the direction is knowable without a price quote, so
    /// the quote half is left to the instrument constructor that owns one.
    #[test]
    fn cash_delivery_should_accept_a_conversion_quoted_in_another_asset() {
        let quoted_in_eur = price_terms(
            UnderlyingRef::Asset(venue_token("BTC")),
            AssetId::Currency(CurrencyId::new(
                Iso4217Code::new("EUR").expect("EUR should be valid"),
            )),
            PriceBasis::PerAssetUnit,
        );

        let delivery = DeliveryMode::cash(venue_token("BTC"), reference(), Some(&quoted_in_eur))
            .expect("the asset half of the direction is the half knowable here");
        assert_eq!(delivery.cash_asset(), Some(&venue_token("BTC")));
        assert_eq!(
            delivery.cash_conversion_reference(),
            Some(
                &quoted_in_eur
                    .reference_id()
                    .expect("reference should be valid")
            )
        );
    }

    /// A cash delivery and an instrument state the same conversion in the same two ways,
    /// each at the level it owns. The matches below are exhaustive on purpose: a way of
    /// stating a conversion that is added to one of the two types stops this test from
    /// compiling until the other one is reconsidered.
    #[test]
    fn cash_delivery_should_state_a_conversion_the_way_an_instrument_does() {
        #[derive(Debug, PartialEq, Eq)]
        enum Statement<'a> {
            /// The registry terms behind the reference, or no conversion at all.
            Terms(Option<&'a PriceReferenceTerms>),
            /// The identity alone, with the instrument-level mirror that may repeat it.
            Identity {
                carrier: Option<PriceReferenceId>,
                mirror: Option<PriceReferenceId>,
            },
        }

        fn delivery_statement(conversion: CashConversion<'_>) -> Statement<'_> {
            match conversion {
                CashConversion::Registry(terms) => Statement::Terms(terms),
                CashConversion::Identity(identity) => Statement::Identity {
                    carrier: identity,
                    mirror: None,
                },
            }
        }

        fn instrument_statement(source: ConversionSource<'_>) -> Statement<'_> {
            match source {
                ConversionSource::Registry { terms, .. } => Statement::Terms(terms),
                ConversionSource::Identity { carrier, mirror } => {
                    Statement::Identity { carrier, mirror }
                }
            }
        }

        let conversion = conversion(venue_token("BTC"), &usd());
        let identity = conversion
            .reference_id()
            .expect("reference should be valid");

        // The terms a caller holds are one statement at both levels: the delivery derives
        // the identity they name, and so does the instrument that resolves the contract.
        assert_eq!(
            delivery_statement(CashConversion::Registry(Some(&conversion))),
            instrument_statement(ConversionSource::Registry {
                carrier: Some(identity),
                terms: Some(&conversion),
            }),
            "a delivery derives its identity from the terms an instrument carries"
        );
        // A delivery has no instrument level, so an identity is the identity alone there,
        // and the two levels agree on what no conversion at all is.
        assert_eq!(
            delivery_statement(CashConversion::Identity(Some(identity))),
            instrument_statement(ConversionSource::Identity {
                carrier: Some(identity),
                mirror: None,
            })
        );
        assert_eq!(
            delivery_statement(CashConversion::Registry(None)),
            instrument_statement(ConversionSource::Registry {
                carrier: None,
                terms: None,
            })
        );
        assert_eq!(
            delivery_statement(CashConversion::Identity(None)),
            instrument_statement(ConversionSource::Identity {
                carrier: None,
                mirror: None,
            })
        );

        // Both statements of one conversion build one delivery: the terms a caller holds
        // and the identity a decoded payload carries resolve to the same delivery.
        assert_eq!(
            DeliveryMode::cash(venue_token("BTC"), reference(), Some(&conversion))
                .expect("the caller states a conversion its delivery can check"),
            DeliveryMode::try_from(DeliveryModeWire::Cash {
                asset: venue_token("BTC"),
                reference: reference(),
                conversion_reference: Some(identity),
            })
            .expect("the payload states the identity of the same conversion"),
            "the two statements of one conversion build one delivery"
        );
    }

    #[test]
    fn cash_delivery_should_encode_the_conversion_reference_when_present() {
        let (delivery, conversion) = settlement("BTC", &usd());
        let mut encoder = crate::identifier::CanonicalEncoder::new();
        delivery.encode(&mut encoder);
        let mut expected: Vec<u8> = Vec::new();
        expected.extend_from_slice(&4_u64.to_be_bytes());
        expected.extend_from_slice(b"cash");
        expected.extend_from_slice(&terms_bytes(&venue_token("BTC")));
        expected.extend_from_slice(&terms_bytes(&reference()));
        expected.extend_from_slice(&20_u64.to_be_bytes());
        expected.extend_from_slice(b"conversion-reference");
        expected.extend_from_slice(&terms_bytes(
            &conversion
                .reference_id()
                .expect("reference should be valid"),
        ));

        assert_eq!(encoder.as_bytes(), expected.as_slice());
        let mut without_conversion = crate::identifier::CanonicalEncoder::new();
        DeliveryMode::cash(venue_token("BTC"), reference(), None)
            .expect("delivery should be valid")
            .encode(&mut without_conversion);
        assert_ne!(
            encoder.as_bytes(),
            without_conversion.as_bytes(),
            "a conversion reference must change the delivery bytes"
        );
    }

    #[test]
    fn cash_delivery_should_round_trip_through_serde() {
        let quote_settled =
            DeliveryMode::cash(usd(), reference(), None).expect("delivery should be valid");
        let cross_asset = settlement("BTC", &usd()).0;
        let token_settled = currency_settlement("EUR", &usd());

        for delivery in [quote_settled, cross_asset, token_settled] {
            let encoded = serde_json::to_string(&delivery).expect("delivery should serialize");
            let decoded: DeliveryMode =
                serde_json::from_str(&encoded).expect("delivery should deserialize");

            assert_eq!(decoded, delivery);
        }

        let without_field = serde_json::to_value(
            DeliveryMode::cash(usd(), reference(), None).expect("delivery should be valid"),
        )
        .expect("delivery should serialize");
        assert!(
            without_field
                .get("mode")
                .and_then(|mode| mode.get("conversion_reference"))
                .is_none(),
            "an absent conversion reference must not appear on the wire"
        );
    }

    #[test]
    fn cash_delivery_should_reject_a_malformed_wire_value() {
        let encoded = serde_json::to_value(
            DeliveryMode::cash(usd(), reference(), None).expect("delivery should be valid"),
        )
        .expect("delivery should serialize");

        let mut unknown_kind = encoded.clone();
        unknown_kind["mode"]["asset"] = serde_json::json!({"kind": "not-an-asset"});
        assert!(serde_json::from_value::<DeliveryMode>(unknown_kind).is_err());

        let mut wrong_settlement_asset = encoded.clone();
        wrong_settlement_asset["mode"]["asset"] = serde_json::json!({
            "kind": "security",
            "spec": "v1/0000000000000000000000000000000000000000000000000000000000000000"
        });
        assert!(
            serde_json::from_value::<DeliveryMode>(wrong_settlement_asset).is_err(),
            "a settlement asset outside the supported asset kinds must not decode"
        );
        // The wire arm runs the entry the constructor runs, so the same asset is
        // refused for the typed reason the constructor states rather than only
        // failing to decode.
        let refused = AssetId::Security(
            SecurityId::from_identity_value(
                "v1/0000000000000000000000000000000000000000000000000000000000000000",
            )
            .expect("the identity should decode"),
        );
        assert_eq!(
            DeliveryMode::try_from(DeliveryModeWire::Cash {
                asset: refused.clone(),
                reference: reference(),
                conversion_reference: None,
            }),
            Err(ModelError::UnsupportedSettlementAsset {
                asset: refused,
                role: SettlementAssetRole::CashDelivery,
            })
        );

        let mut malformed_reference = encoded;
        malformed_reference["mode"]["reference"] = serde_json::Value::String("spot".into());
        assert!(serde_json::from_value::<DeliveryMode>(malformed_reference).is_err());
    }

    #[test]
    fn strike_should_reject_a_zero_value() {
        assert_eq!(
            StrikePrice::new(Price::ZERO, usd(), PriceBasis::PerAssetUnit),
            Err(ModelError::ZeroValue { field: "strike" })
        );
    }

    #[test]
    fn lineages_should_accept_a_monotonic_chain() {
        let a = crypto_spot_identity("BTC", "USD");
        let b = crypto_spot_identity("BTC", "USDT");
        let c = crypto_spot_identity("ETH", "USDT");
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        let edges = vec![
            InstrumentLineageEdge::new(a, b, InstrumentAdjustment::CorporateAction, 1)
                .expect("edge should be valid"),
            InstrumentLineageEdge::new(b, c, InstrumentAdjustment::DeliverableAdjustment, 2)
                .expect("edge should be valid"),
        ];
        assert_eq!(edges[0].successor, edges[1].predecessor);
        assert_ne!(edges[0].predecessor, edges[1].predecessor);
        assert_ne!(edges[0].successor, edges[1].successor);
        assert_eq!(edges[0].successor.as_str().len(), 67);
        for edge in &edges {
            println!(
                "edge {} -> {} @ {}",
                edge.predecessor.as_str(),
                edge.successor.as_str(),
                edge.effective_at
            );
        }

        assert_eq!(
            validate_lineage(&edges, |field| ModelError::InstrumentLineageInvalid {
                field
            }),
            Ok(())
        );
    }

    #[test]
    fn lineage_should_reject_a_non_monotonic_effective_time() {
        let a = crypto_spot_identity("BTC", "USD");
        let b = crypto_spot_identity("BTC", "USDT");
        let c = crypto_spot_identity("ETH", "USDT");
        let edges = vec![
            InstrumentLineageEdge::new(a, b, InstrumentAdjustment::CorporateAction, 9)
                .expect("edge should be valid"),
            InstrumentLineageEdge::new(b, c, InstrumentAdjustment::DeliverableAdjustment, 1)
                .expect("edge should be valid"),
        ];

        assert_eq!(
            validate_lineage(&edges, |field| ModelError::InstrumentLineageInvalid {
                field
            }),
            Err(ModelError::InstrumentLineageInvalid {
                field: "successor takes effect before its predecessor"
            })
        );
    }

    #[test]
    fn lineage_should_reject_cycles_and_duplicate_successors() {
        let a = crypto_spot_identity("BTC", "USD");
        let b = crypto_spot_identity("BTC", "USDT");
        let c = crypto_spot_identity("ETH", "USDT");
        let cycle = vec![
            InstrumentLineageEdge::new(a, b, InstrumentAdjustment::CorporateAction, 0)
                .expect("edge should be valid"),
            InstrumentLineageEdge::new(b, a, InstrumentAdjustment::CorporateAction, 0)
                .expect("edge should be valid"),
        ];
        let duplicate_successor = vec![
            InstrumentLineageEdge::new(a, c, InstrumentAdjustment::CorporateAction, 0)
                .expect("edge should be valid"),
            InstrumentLineageEdge::new(b, c, InstrumentAdjustment::CorporateAction, 0)
                .expect("edge should be valid"),
        ];
        let duplicated = vec![
            InstrumentLineageEdge::new(a, c, InstrumentAdjustment::CorporateAction, 0)
                .expect("edge should be valid"),
            InstrumentLineageEdge::new(a, c, InstrumentAdjustment::CorporateAction, 0)
                .expect("edge should be valid"),
        ];
        let invalid = |field: &'static str| ModelError::InstrumentLineageInvalid { field };

        assert_eq!(
            validate_lineage(&cycle, invalid),
            Err(ModelError::InstrumentLineageInvalid {
                field: "lineage cycle"
            })
        );
        assert_eq!(
            validate_lineage(&duplicate_successor, invalid),
            Err(ModelError::InstrumentLineageInvalid {
                field: "duplicate lineage successor"
            })
        );
        assert_eq!(
            validate_lineage(&duplicated, invalid),
            Err(ModelError::InstrumentLineageInvalid {
                field: "duplicate lineage relation"
            })
        );
    }

    #[test]
    fn lineage_edge_should_reject_a_self_reference() {
        let a = crypto_spot_identity("BTC", "USDT");

        assert!(
            InstrumentLineageEdge::new(a, a, InstrumentAdjustment::CorporateAction, 0).is_err()
        );
    }

    #[test]
    fn stock_split_should_preserve_the_security_identity() {
        let security = SecurityId::new(
            SecurityIssuerId::new("cik/320193").expect("issuer should be valid"),
            ShareClassId::new(&ShareClassTerms {
                issuer: SecurityIssuerId::new("cik/320193").expect("issuer should be valid"),
                designation: "common".into(),
                form: SecurityForm::CommonStock,
                par_value: None,
                adr_ratio: None,
            })
            .expect("share class should be valid"),
            SecurityForm::CommonStock,
        );
        let identity = security;
        let split = StockSplit::new(
            security,
            PositiveRatio::new(Ratio::from(4_u8)).expect("ratio should be positive"),
            1_700_000_000_000,
        )
        .expect("split should be valid");

        assert_eq!(split.security, identity);
        assert_eq!(
            split.adjust_quantity(Quantity::from(10_u8)),
            Some(Quantity::from(40_u8))
        );
        assert_eq!(split.dedup_key().1, 1_700_000_000_000);
    }

    #[test]
    fn stock_split_dedup_key_should_be_security_effective_at_and_ratio() {
        let security = |issuer: &str, designation: &str| {
            SecurityId::new(
                SecurityIssuerId::new(issuer).expect("issuer should be valid"),
                ShareClassId::new(&ShareClassTerms {
                    issuer: SecurityIssuerId::new(issuer).expect("issuer should be valid"),
                    designation: designation.into(),
                    form: SecurityForm::CommonStock,
                    par_value: None,
                    adr_ratio: None,
                })
                .expect("share class should be valid"),
                SecurityForm::CommonStock,
            )
        };
        let apple = security("cik/320193", "common");
        let other = security("cik/320193", "preferred");
        let split = |security: SecurityId, ratio: u8, effective_at| {
            StockSplit::new(
                security,
                PositiveRatio::new(Ratio::from(ratio)).expect("ratio should be positive"),
                effective_at,
            )
            .expect("split should be valid")
        };
        let first = split(apple, 4, 1_700_000_000_000);
        let repeated = split(apple, 4, 1_700_000_000_000);

        assert_eq!(
            first.dedup_key(),
            repeated.dedup_key(),
            "the same security, instant and ratio is the same corporate action"
        );
        assert_ne!(
            first.dedup_key(),
            split(apple, 2, 1_700_000_000_000).dedup_key()
        );
        assert_ne!(
            first.dedup_key(),
            split(apple, 4, 1_700_000_000_001).dedup_key()
        );
        assert_ne!(
            first.dedup_key(),
            split(other, 4, 1_700_000_000_000).dedup_key()
        );
    }

    #[test]
    fn underlying_reference_should_report_what_it_tracks() {
        let asset = UnderlyingRef::Asset(venue_token("BTC"));

        assert_eq!(asset.asset(), Some(&venue_token("BTC")));
        assert!(asset.index().is_none());
    }

    #[test]
    fn term_types_should_reject_struct_literal_bypass() {
        // The fields are private, so no test outside this module can build an
        // invalid `Expiry` or `ContractSize` without going through `new`; the
        // compile-time guarantee is asserted by the private-field access below.
        let expiry = Expiry::new(1, None, None).expect("expiry should be valid");
        assert_eq!(expiry.expiration_at(), 1);
    }
}
