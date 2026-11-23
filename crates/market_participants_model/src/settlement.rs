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
    pub fn asset_quantity(quantity: Quantity, asset: AssetId) -> Result<Self, ModelError> {
        crate::error::require_positive_quantity(quantity, "contract_unit.quantity")?;
        Ok(Self::AssetQuantity { quantity, asset })
    }

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

    pub fn quantity(&self) -> Quantity {
        match self {
            Self::AssetQuantity { quantity, .. } => *quantity,
            Self::QuoteNotionalPerIndexPoint { amount, .. } => *amount,
        }
    }

    pub fn denomination(&self) -> Option<&AssetId> {
        match self {
            Self::AssetQuantity { asset, .. } => Some(asset),
            Self::QuoteNotionalPerIndexPoint { .. } => None,
        }
    }

    pub fn index(&self) -> Option<&IndexId> {
        match self {
            Self::QuoteNotionalPerIndexPoint { index, .. } => Some(index),
            Self::AssetQuantity { .. } => None,
        }
    }

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

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ContractSize {
    #[serde(rename = "unit")]
    unit: ContractUnit,
}

impl ContractSize {
    pub fn new(unit: ContractUnit) -> Result<Self, ModelError> {
        crate::error::require_positive_quantity(unit.quantity(), "contract_unit.quantity")?;
        Ok(Self { unit })
    }

    pub fn unit(&self) -> &ContractUnit {
        &self.unit
    }

    pub fn quantity(&self) -> Quantity {
        self.unit.quantity()
    }

    pub fn denomination(&self) -> Option<&AssetId> {
        self.unit.denomination()
    }

    pub fn index(&self) -> Option<&IndexId> {
        self.unit.index()
    }

    pub fn quote_asset(&self) -> Option<&AssetId> {
        self.unit.quote_asset()
    }

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

    pub const fn expiration_at(&self) -> UnixMillis {
        self.expiration_at
    }

    pub const fn last_trading_at(&self) -> Option<UnixMillis> {
        self.last_trading_at
    }

    pub const fn settlement_at(&self) -> Option<UnixMillis> {
        self.settlement_at
    }

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum DeliveryDirection {
    #[serde(rename = "receive")]
    Receive,
    #[serde(rename = "deliver")]
    Deliver,
}

impl DeliveryDirection {
    pub const fn opposite(self) -> Self {
        match self {
            Self::Receive => Self::Deliver,
            Self::Deliver => Self::Receive,
        }
    }

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

    pub fn asset(&self) -> &AssetId {
        &self.asset
    }

    pub const fn quantity(&self) -> Quantity {
        self.quantity
    }

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

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(tag = "kind", content = "mode")]
pub enum DeliveryMode {
    #[serde(rename = "cash")]
    Cash {
        #[serde(rename = "asset")]
        asset: AssetId,
        #[serde(rename = "reference")]
        reference: PriceReferenceId,
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

enum CashConversion<'a> {
    Registry(Option<&'a PriceReferenceTerms>),
    Identity(Option<PriceReferenceId>),
}

impl DeliveryMode {
    pub fn cash(
        asset: AssetId,
        reference: PriceReferenceId,
        conversion: Option<&PriceReferenceTerms>,
    ) -> Result<Self, ModelError> {
        Self::build_cash(asset, reference, CashConversion::Registry(conversion))
    }

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

    pub fn cash_asset(&self) -> Option<&AssetId> {
        match self {
            Self::Cash { asset, .. } => Some(asset),
            Self::Physical { .. } => None,
        }
    }

    pub fn cash_reference(&self) -> Option<&PriceReferenceId> {
        match self {
            Self::Cash { reference, .. } => Some(reference),
            Self::Physical { .. } => None,
        }
    }

    pub fn cash_conversion_reference(&self) -> Option<&PriceReferenceId> {
        match self {
            Self::Cash {
                conversion_reference,
                ..
            } => conversion_reference.as_ref(),
            Self::Physical { .. } => None,
        }
    }

    pub fn deliveries(&self) -> &[DeliveryLeg] {
        match self {
            Self::Physical { deliveries } => deliveries,
            Self::Cash { .. } => &[],
        }
    }

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OptionSettlementObservation {
    Quote,
    Converted {
        conversion_reference: PriceReferenceId,
        amount: AssetAmount,
    },
}

impl OptionSettlementObservation {
    pub fn converted(conversion_reference: PriceReferenceId, amount: AssetAmount) -> Self {
        Self::Converted {
            conversion_reference,
            amount,
        }
    }

    pub fn converted_amount(&self) -> Option<&AssetAmount> {
        match self {
            Self::Quote => None,
            Self::Converted { amount, .. } => Some(amount),
        }
    }

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

    pub const fn value(&self) -> Price {
        self.value
    }

    pub fn quote_asset(&self) -> &AssetId {
        &self.quote_asset
    }

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum OptionRight {
    #[serde(rename = "call")]
    Call,
    #[serde(rename = "put")]
    Put,
}

impl OptionRight {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Put => "put",
        }
    }

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
    pub fn bermudan(exercise_at: Vec<UnixMillis>, expiry: &Expiry) -> Result<Self, ModelError> {
        let style = Self::build(exercise_at)?;
        style.validate(expiry)?;
        Ok(style)
    }

    fn build(exercise_at: Vec<UnixMillis>) -> Result<Self, ModelError> {
        require_strictly_sorted(&exercise_at, "exercise_at")?;
        for instant in &exercise_at {
            require_non_negative_time(*instant, "exercise_at")?;
        }
        Ok(Self::Bermudan { exercise_at })
    }

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

    pub fn exercise_at(&self) -> &[UnixMillis] {
        match self {
            Self::Bermudan { exercise_at } => exercise_at,
            Self::American | Self::European => &[],
        }
    }

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

    pub fn adjust_quantity(&self, quantity: Quantity) -> Option<Quantity> {
        self.new_shares_per_old_share
            .ratio()
            .checked_mul_quantity(quantity)
    }

    pub fn dedup_key(&self) -> (SecurityId, UnixMillis, Ratio) {
        (
            self.security,
            self.effective_at,
            self.new_shares_per_old_share.ratio(),
        )
    }
}

pub trait LineageRelation {
    fn predecessor(&self) -> IdentityText;

    fn successor(&self) -> IdentityText;

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

    fn conversion(settlement: AssetId, price_quote: &AssetId) -> PriceReferenceTerms {
        price_terms(
            UnderlyingRef::Asset(settlement),
            price_quote.clone(),
            PriceBasis::PerAssetUnit,
        )
    }

    fn settlement(code: &str, price_quote: &AssetId) -> (DeliveryMode, PriceReferenceTerms) {
        let conversion = conversion(venue_token(code), price_quote);
        let delivery = DeliveryMode::cash(venue_token(code), reference(), Some(&conversion))
            .expect("delivery should be valid");
        (delivery, conversion)
    }

    fn terms_bytes<T: CanonicalTerms>(term: &T) -> Vec<u8> {
        let mut encoder = crate::identifier::CanonicalEncoder::new();
        encoder.write_terms(term);
        encoder.as_bytes().to_vec()
    }

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

    #[test]
    fn cash_delivery_should_state_a_conversion_the_way_an_instrument_does() {
        #[derive(Debug, PartialEq, Eq)]
        enum Statement<'a> {
            Terms(Option<&'a PriceReferenceTerms>),
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

        assert_eq!(
            delivery_statement(CashConversion::Registry(Some(&conversion))),
            instrument_statement(ConversionSource::Registry {
                carrier: Some(identity),
                terms: Some(&conversion),
            }),
            "a delivery derives its identity from the terms an instrument carries"
        );
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
        let expiry = Expiry::new(1, None, None).expect("expiry should be valid");
        assert_eq!(expiry.expiration_at(), 1);
    }
}
