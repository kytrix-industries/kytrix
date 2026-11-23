use std::fmt;

use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, MapAccess, SeqAccess, Visitor, value::MapAccessDeserializer},
};

use crate::{
    asset::{AssetId, DerivativeUnderlying, UnderlyingRef},
    decimal::{Price, SignedQuantity, UnixMillis},
    error::{
        ConversionDirection, IndexField, ModelError, QuantityUnitExpectation, SettlementAssetRole,
        require_non_negative_time, require_ordered_time,
    },
    identifier::{CanonicalEncoder, CanonicalTerms},
    listing::venue::PriceBasis,
    market::account::AssetAmount,
    outcome::{OutcomeGroupId, OutcomeMemberId, OutcomeSideId},
    reference::{PriceReferenceId, PriceReferenceTerms},
    settlement::{
        ContractSize, DeliveryDirection, DeliveryMode, DerivativePnlModel, ExerciseStyle, Expiry,
        InstrumentId, OptionRight, OptionSettlementObservation, StrikePrice,
    },
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct SpotTerms {
    #[serde(rename = "base")]
    base: AssetId,
    #[serde(rename = "quote")]
    quote: AssetId,
}

impl SpotTerms {
    pub fn new(base: AssetId, quote: AssetId) -> Result<Self, ModelError> {
        if base == quote {
            return Err(ModelError::IdenticalSpotAssets);
        }
        Ok(Self { base, quote })
    }

    pub fn base(&self) -> &AssetId {
        &self.base
    }

    pub fn quote(&self) -> &AssetId {
        &self.quote
    }
}

impl CanonicalTerms for SpotTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_terms(&self.base);
        encoder.write_terms(&self.quote);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct PerpetualTerms {
    #[serde(rename = "underlying")]
    underlying: DerivativeUnderlying,
    #[serde(rename = "price_quote")]
    price_quote: AssetId,
    #[serde(rename = "settlement_asset")]
    settlement_asset: AssetId,
    #[serde(rename = "contract_size")]
    contract_size: ContractSize,
    #[serde(rename = "pnl_model")]
    pnl_model: DerivativePnlModel,
    #[serde(
        rename = "conversion_reference",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    conversion_reference: Option<PriceReferenceId>,
}

impl PerpetualTerms {
    pub fn new(
        underlying: DerivativeUnderlying,
        price_quote: AssetId,
        settlement_asset: AssetId,
        contract_size: ContractSize,
        pnl_model: DerivativePnlModel,
        conversion: Option<&PriceReferenceTerms>,
    ) -> Result<Self, ModelError> {
        Self::build(
            underlying,
            price_quote,
            settlement_asset,
            contract_size,
            pnl_model,
            ConversionSource::Registry {
                carrier: conversion.map(|terms| terms.reference_id()).transpose()?,
                terms: conversion,
            },
        )
    }

    pub(crate) fn with_conversion_reference(
        underlying: DerivativeUnderlying,
        price_quote: AssetId,
        settlement_asset: AssetId,
        contract_size: ContractSize,
        pnl_model: DerivativePnlModel,
        conversion_reference: Option<PriceReferenceId>,
    ) -> Result<Self, ModelError> {
        Self::build(
            underlying,
            price_quote,
            settlement_asset,
            contract_size,
            pnl_model,
            ConversionSource::Identity {
                carrier: conversion_reference,
                mirror: None,
            },
        )
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        Self::build(
            self.underlying.clone(),
            self.price_quote.clone(),
            self.settlement_asset.clone(),
            self.contract_size.clone(),
            self.pnl_model,
            ConversionSource::Identity {
                carrier: self.conversion_reference,
                mirror: None,
            },
        )
        .map(|_| ())
    }

    fn build(
        underlying: DerivativeUnderlying,
        price_quote: AssetId,
        settlement_asset: AssetId,
        contract_size: ContractSize,
        pnl_model: DerivativePnlModel,
        conversion: ConversionSource<'_>,
    ) -> Result<Self, ModelError> {
        validate_derivative_terms(
            &underlying,
            &price_quote,
            &settlement_asset,
            &contract_size,
            pnl_model,
            None,
        )?;
        let conversion_reference = resolve_conversion(
            !matches!(pnl_model, DerivativePnlModel::Linear)
                && convertible(false, &settlement_asset, &price_quote),
            conversion,
            &settlement_asset,
            &price_quote,
        )?;
        Ok(Self {
            underlying,
            price_quote,
            settlement_asset,
            contract_size,
            pnl_model,
            conversion_reference,
        })
    }

    pub fn underlying(&self) -> &DerivativeUnderlying {
        &self.underlying
    }

    pub fn price_quote(&self) -> &AssetId {
        &self.price_quote
    }

    pub fn settlement_asset(&self) -> &AssetId {
        &self.settlement_asset
    }

    pub fn contract_size(&self) -> &ContractSize {
        &self.contract_size
    }

    pub fn pnl_model(&self) -> DerivativePnlModel {
        self.pnl_model
    }

    pub fn conversion_reference(&self) -> Option<&PriceReferenceId> {
        self.conversion_reference.as_ref()
    }
}

impl CanonicalTerms for PerpetualTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_terms(&self.underlying);
        encoder.write_terms(&self.price_quote);
        encoder.write_terms(&self.settlement_asset);
        encoder.write_terms(&self.contract_size);
        encoder.write_terms(&self.pnl_model);
        if let Some(conversion) = &self.conversion_reference {
            encoder.write_label("conversion-reference");
            encoder.write_terms(conversion);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct FutureTerms {
    #[serde(rename = "underlying")]
    underlying: DerivativeUnderlying,
    #[serde(rename = "price_quote")]
    price_quote: AssetId,
    #[serde(rename = "pnl_model")]
    pnl_model: DerivativePnlModel,
    #[serde(rename = "contract_size")]
    contract_size: ContractSize,
    #[serde(rename = "expiry")]
    expiry: Expiry,
    #[serde(rename = "first_notice_at")]
    first_notice_at: Option<UnixMillis>,
    #[serde(rename = "delivery")]
    delivery: DeliveryMode,
}

impl FutureTerms {
    pub fn new(
        underlying: DerivativeUnderlying,
        price_quote: AssetId,
        pnl_model: DerivativePnlModel,
        contract_size: ContractSize,
        expiry: Expiry,
        first_notice_at: Option<UnixMillis>,
        delivery: DeliveryMode,
        conversion: Option<&PriceReferenceTerms>,
    ) -> Result<Self, ModelError> {
        let carrier = delivery.cash_conversion_reference().cloned();
        Self::build(
            underlying,
            price_quote,
            pnl_model,
            contract_size,
            expiry,
            first_notice_at,
            delivery,
            ConversionSource::Registry {
                carrier,
                terms: conversion,
            },
        )
    }

    pub(crate) fn with_conversion_reference(
        underlying: DerivativeUnderlying,
        price_quote: AssetId,
        pnl_model: DerivativePnlModel,
        contract_size: ContractSize,
        expiry: Expiry,
        first_notice_at: Option<UnixMillis>,
        delivery: DeliveryMode,
        conversion_reference: Option<PriceReferenceId>,
    ) -> Result<Self, ModelError> {
        let carrier = delivery.cash_conversion_reference().cloned();
        Self::build(
            underlying,
            price_quote,
            pnl_model,
            contract_size,
            expiry,
            first_notice_at,
            delivery,
            ConversionSource::Identity {
                carrier,
                mirror: conversion_reference,
            },
        )
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        Self::build(
            self.underlying.clone(),
            self.price_quote.clone(),
            self.pnl_model,
            self.contract_size.clone(),
            self.expiry,
            self.first_notice_at,
            self.delivery.clone(),
            ConversionSource::Identity {
                carrier: self.delivery.cash_conversion_reference().cloned(),
                mirror: None,
            },
        )
        .map(|_| ())
    }

    fn build(
        underlying: DerivativeUnderlying,
        price_quote: AssetId,
        pnl_model: DerivativePnlModel,
        contract_size: ContractSize,
        expiry: Expiry,
        first_notice_at: Option<UnixMillis>,
        delivery: DeliveryMode,
        conversion: ConversionSource<'_>,
    ) -> Result<Self, ModelError> {
        delivery.validate()?;
        expiry.validate()?;
        if let Some(first_notice_at) = first_notice_at {
            require_non_negative_time(first_notice_at, "first_notice_at")?;
            require_ordered_time(
                first_notice_at,
                expiry.last_tradable_at(),
                "first_notice_at",
                "last_trading_at",
            )?;
        }
        let settlement_asset = delivery
            .cash_asset()
            .cloned()
            .unwrap_or_else(|| price_quote.clone());
        resolve_conversion(
            convertible(delivery.is_physical(), &settlement_asset, &price_quote),
            conversion,
            &settlement_asset,
            &price_quote,
        )?;
        validate_derivative_terms(
            &underlying,
            &price_quote,
            &settlement_asset,
            &contract_size,
            pnl_model,
            Some(&delivery),
        )?;
        Ok(Self {
            underlying,
            price_quote,
            pnl_model,
            contract_size,
            expiry,
            first_notice_at,
            delivery,
        })
    }

    pub fn underlying(&self) -> &DerivativeUnderlying {
        &self.underlying
    }

    pub fn price_quote(&self) -> &AssetId {
        &self.price_quote
    }

    pub fn pnl_model(&self) -> DerivativePnlModel {
        self.pnl_model
    }

    pub fn contract_size(&self) -> &ContractSize {
        &self.contract_size
    }

    pub fn expiry(&self) -> &Expiry {
        &self.expiry
    }

    pub fn first_notice_at(&self) -> Option<UnixMillis> {
        self.first_notice_at
    }

    pub fn delivery(&self) -> &DeliveryMode {
        &self.delivery
    }
}

impl CanonicalTerms for FutureTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_terms(&self.underlying);
        encoder.write_terms(&self.price_quote);
        encoder.write_terms(&self.pnl_model);
        encoder.write_terms(&self.contract_size);
        encoder.write_terms(&self.expiry);
        encoder.write_optional(self.first_notice_at.as_ref(), |encoder, value| {
            encoder.write_i64(*value);
        });
        encoder.write_terms(&self.delivery);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct OptionTerms {
    #[serde(rename = "underlying")]
    underlying: DerivativeUnderlying,
    #[serde(rename = "price_quote")]
    price_quote: AssetId,
    #[serde(rename = "right")]
    right: OptionRight,
    #[serde(rename = "strike")]
    strike: StrikePrice,
    #[serde(rename = "expiry")]
    expiry: Expiry,
    #[serde(rename = "exercise")]
    exercise: ExerciseStyle,
    #[serde(rename = "contract_size")]
    contract_size: ContractSize,
    #[serde(rename = "delivery")]
    delivery: DeliveryMode,
}

impl OptionTerms {
    pub fn new(
        underlying: DerivativeUnderlying,
        price_quote: AssetId,
        right: OptionRight,
        strike: StrikePrice,
        expiry: Expiry,
        exercise: ExerciseStyle,
        contract_size: ContractSize,
        delivery: DeliveryMode,
        conversion: Option<&PriceReferenceTerms>,
    ) -> Result<Self, ModelError> {
        let carrier = delivery.cash_conversion_reference().cloned();
        Self::build(
            underlying,
            price_quote,
            right,
            strike,
            expiry,
            exercise,
            contract_size,
            delivery,
            ConversionSource::Registry {
                carrier,
                terms: conversion,
            },
        )
    }

    pub(crate) fn with_conversion_reference(
        underlying: DerivativeUnderlying,
        price_quote: AssetId,
        right: OptionRight,
        strike: StrikePrice,
        expiry: Expiry,
        exercise: ExerciseStyle,
        contract_size: ContractSize,
        delivery: DeliveryMode,
        conversion_reference: Option<PriceReferenceId>,
    ) -> Result<Self, ModelError> {
        let carrier = delivery.cash_conversion_reference().cloned();
        Self::build(
            underlying,
            price_quote,
            right,
            strike,
            expiry,
            exercise,
            contract_size,
            delivery,
            ConversionSource::Identity {
                carrier,
                mirror: conversion_reference,
            },
        )
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        Self::build(
            self.underlying.clone(),
            self.price_quote.clone(),
            self.right,
            self.strike.clone(),
            self.expiry,
            self.exercise.clone(),
            self.contract_size.clone(),
            self.delivery.clone(),
            ConversionSource::Identity {
                carrier: self.delivery.cash_conversion_reference().cloned(),
                mirror: None,
            },
        )
        .map(|_| ())
    }

    fn build(
        underlying: DerivativeUnderlying,
        price_quote: AssetId,
        right: OptionRight,
        strike: StrikePrice,
        expiry: Expiry,
        exercise: ExerciseStyle,
        contract_size: ContractSize,
        delivery: DeliveryMode,
        conversion: ConversionSource<'_>,
    ) -> Result<Self, ModelError> {
        delivery.validate()?;
        expiry.validate()?;
        exercise.validate(&expiry)?;
        if strike.quote_asset() != &price_quote {
            return Err(ModelError::StrikeQuoteMismatch);
        }
        if strike.basis() != &expected_price_basis(&underlying, &contract_size) {
            return Err(ModelError::IncompatiblePriceBasis);
        }
        let settlement_asset = delivery.cash_asset().unwrap_or(&price_quote).clone();
        resolve_conversion(
            convertible(delivery.is_physical(), &settlement_asset, &price_quote),
            conversion,
            &settlement_asset,
            &price_quote,
        )?;
        validate_contract_unit(&underlying, &price_quote, &contract_size)?;
        if delivery.is_physical() {
            validate_option_deliveries(&underlying, &delivery, &contract_size, right)?;
        }
        Ok(Self {
            underlying,
            price_quote,
            right,
            strike,
            expiry,
            exercise,
            contract_size,
            delivery,
        })
    }

    pub fn underlying(&self) -> &DerivativeUnderlying {
        &self.underlying
    }

    pub fn price_quote(&self) -> &AssetId {
        &self.price_quote
    }

    pub fn right(&self) -> OptionRight {
        self.right
    }

    pub fn strike(&self) -> &StrikePrice {
        &self.strike
    }

    pub fn expiry(&self) -> &Expiry {
        &self.expiry
    }

    pub fn exercise(&self) -> &ExerciseStyle {
        &self.exercise
    }

    pub fn contract_size(&self) -> &ContractSize {
        &self.contract_size
    }

    pub fn delivery(&self) -> &DeliveryMode {
        &self.delivery
    }

    pub fn conversion_reference(&self) -> Option<&PriceReferenceId> {
        self.delivery.cash_conversion_reference()
    }

    pub fn cash_payoff(
        &self,
        reference: Price,
        settlement: OptionSettlementObservation,
    ) -> Result<AssetAmount, ModelError> {
        let Some(settlement_asset) = self.delivery.cash_asset() else {
            return Err(ModelError::IncompatibleDeliveryMode);
        };
        let Some(unit) = self.contract_size.cash_quantity() else {
            return Err(ModelError::IndexPointPayoffUnsupported);
        };
        let Some(underlying_asset) = self.underlying.reference().asset() else {
            return Err(ModelError::UnderlyingWithoutAsset);
        };
        if let Some(denomination) = self.contract_size.denomination()
            && denomination != underlying_asset
        {
            return Err(ModelError::PayoffUnitMismatch {
                underlying: underlying_asset.clone(),
                denomination: denomination.clone(),
            });
        }
        settlement.validate_against(self.delivery.cash_conversion_reference(), settlement_asset)?;
        let payoff = match settlement.converted_amount() {
            Some(amount) => amount.value,
            None => {
                let intrinsic = unit
                    .checked_mul_price(Price::from_inner(
                        self.right
                            .intrinsic_value(reference, self.strike.value())
                            .inner(),
                    ))
                    .ok_or(ModelError::DecimalOverflow)?;
                SignedQuantity::from_inner(
                    i128::try_from(intrinsic.inner()).map_err(|_| ModelError::DecimalOverflow)?,
                )
            }
        };
        Ok(AssetAmount {
            asset: settlement_asset.clone(),
            value: payoff,
        })
    }
}

impl CanonicalTerms for OptionTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_terms(&self.underlying);
        encoder.write_terms(&self.price_quote);
        encoder.write_terms(&self.right);
        encoder.write_terms(&self.strike);
        encoder.write_terms(&self.expiry);
        encoder.write_terms(&self.exercise);
        encoder.write_terms(&self.contract_size);
        encoder.write_terms(&self.delivery);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct OutcomeTerms {
    #[serde(rename = "group")]
    group: OutcomeGroupId,
    #[serde(rename = "member")]
    member: OutcomeMemberId,
    #[serde(rename = "side")]
    side: OutcomeSideId,
}

impl OutcomeTerms {
    pub fn new(group: OutcomeGroupId, member: OutcomeMemberId, side: OutcomeSideId) -> Self {
        Self {
            group,
            member,
            side,
        }
    }

    pub fn group(&self) -> &OutcomeGroupId {
        &self.group
    }

    pub fn member(&self) -> &OutcomeMemberId {
        &self.member
    }

    pub fn side(&self) -> &OutcomeSideId {
        &self.side
    }
}

impl CanonicalTerms for OutcomeTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_terms(&self.group);
        encoder.write_terms(&self.member);
        encoder.write_terms(&self.side);
    }
}

#[derive(Deserialize)]
struct OutcomeTermsWire {
    #[serde(rename = "group")]
    group: OutcomeGroupId,
    #[serde(rename = "member")]
    member: OutcomeMemberId,
    #[serde(rename = "side")]
    side: OutcomeSideId,
}

impl TryFrom<OutcomeTermsWire> for OutcomeTerms {
    type Error = ModelError;

    fn try_from(wire: OutcomeTermsWire) -> Result<Self, Self::Error> {
        Ok(Self::new(wire.group, wire.member, wire.side))
    }
}

const OUTCOME_TERMS_FIELDS: &[&str] = &["group", "member", "side"];

struct OutcomeTermsVisitor;

impl<'de> Visitor<'de> for OutcomeTermsVisitor {
    type Value = OutcomeTerms;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("struct OutcomeTerms")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let group = sequence.next_element()?.ok_or_else(|| {
            de::Error::invalid_length(0usize, &"struct OutcomeTerms with 3 elements")
        })?;
        let member = sequence.next_element()?.ok_or_else(|| {
            de::Error::invalid_length(1usize, &"struct OutcomeTerms with 3 elements")
        })?;
        let side = sequence.next_element()?.ok_or_else(|| {
            de::Error::invalid_length(2usize, &"struct OutcomeTerms with 3 elements")
        })?;
        Self::Value::try_from(OutcomeTermsWire {
            group,
            member,
            side,
        })
        .map_err(de::Error::custom)
    }

    fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let wire = OutcomeTermsWire::deserialize(MapAccessDeserializer::new(map))?;
        Self::Value::try_from(wire).map_err(de::Error::custom)
    }
}

impl<'de> Deserialize<'de> for OutcomeTerms {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_struct("OutcomeTerms", OUTCOME_TERMS_FIELDS, OutcomeTermsVisitor)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum InstrumentKind {
    #[serde(rename = "spot")]
    Spot,
    #[serde(rename = "perpetual")]
    Perpetual,
    #[serde(rename = "future")]
    Future,
    #[serde(rename = "option")]
    Option,
    #[serde(rename = "outcome")]
    Outcome,
}

impl InstrumentKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Spot => "spot",
            Self::Perpetual => "perpetual",
            Self::Future => "future",
            Self::Option => "option",
            Self::Outcome => "outcome",
        }
    }

    pub const fn is_derivative(self) -> bool {
        matches!(self, Self::Perpetual | Self::Future | Self::Option)
    }
}

impl fmt::Display for InstrumentKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InstrumentTerms {
    Spot(SpotTerms),
    Perpetual(PerpetualTerms),
    Future(FutureTerms),
    Option(OptionTerms),
    Outcome(OutcomeTerms),
}

impl InstrumentTerms {
    pub const fn kind(&self) -> InstrumentKind {
        match self {
            Self::Spot(_) => InstrumentKind::Spot,
            Self::Perpetual(_) => InstrumentKind::Perpetual,
            Self::Future(_) => InstrumentKind::Future,
            Self::Option(_) => InstrumentKind::Option,
            Self::Outcome(_) => InstrumentKind::Outcome,
        }
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        match self {
            Self::Spot(terms) => {
                if terms.base() == terms.quote() {
                    Err(ModelError::IdenticalSpotAssets)
                } else {
                    Ok(())
                }
            }
            Self::Perpetual(terms) => terms.validate(),
            Self::Future(terms) => terms.validate(),
            Self::Option(terms) => terms.validate(),
            Self::Outcome(_) => Ok(()),
        }
    }

    pub fn price_basis(&self) -> Option<PriceBasis> {
        match self {
            Self::Future(terms) => Some(expected_price_basis(
                &terms.underlying,
                &terms.contract_size,
            )),
            Self::Option(terms) => Some(expected_price_basis(
                &terms.underlying,
                &terms.contract_size,
            )),
            Self::Perpetual(terms) => Some(expected_price_basis(
                &terms.underlying,
                &terms.contract_size,
            )),
            Self::Spot(_) | Self::Outcome(_) => None,
        }
    }
}

impl CanonicalTerms for InstrumentTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        match self {
            Self::Spot(terms) => {
                encoder.write_label("spot");
                encoder.write_terms(terms);
            }
            Self::Perpetual(terms) => {
                encoder.write_label("perpetual");
                encoder.write_terms(terms);
            }
            Self::Future(terms) => {
                encoder.write_label("future");
                encoder.write_terms(terms);
            }
            Self::Option(terms) => {
                encoder.write_label("option");
                encoder.write_terms(terms);
            }
            Self::Outcome(terms) => {
                encoder.write_label("outcome");
                encoder.write_terms(terms);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Instrument {
    id: InstrumentId,
    terms: InstrumentTerms,
}

impl Instrument {
    pub fn new(terms: InstrumentTerms) -> Result<Self, ModelError> {
        terms.validate()?;
        let id = InstrumentId::of_terms(&terms);
        Ok(Self { id, terms })
    }

    pub fn id(&self) -> &InstrumentId {
        &self.id
    }

    pub fn terms(&self) -> InstrumentTerms {
        self.terms.clone()
    }

    pub fn kind(&self) -> InstrumentKind {
        self.terms.kind()
    }
}

impl fmt::Display for Instrument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.kind().label(), self.id)
    }
}

impl Serialize for Instrument {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        InstrumentWireDto::from(&self.terms).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Instrument {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = InstrumentWireDto::deserialize(deserializer)?;
        let terms = InstrumentTerms::try_from(wire).map_err(de::Error::custom)?;
        Self::new(terms).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "spec")]
enum InstrumentWireDto {
    #[serde(rename = "spot")]
    Spot {
        #[serde(rename = "base")]
        base: AssetId,
        #[serde(rename = "quote")]
        quote: AssetId,
    },
    #[serde(rename = "perpetual")]
    Perpetual {
        #[serde(rename = "underlying")]
        underlying: DerivativeUnderlying,
        #[serde(rename = "price_quote")]
        price_quote: AssetId,
        #[serde(rename = "settlement_asset")]
        settlement_asset: AssetId,
        #[serde(rename = "contract_size")]
        contract_size: ContractSize,
        #[serde(rename = "pnl_model")]
        pnl_model: DerivativePnlModel,
        #[serde(
            rename = "conversion_reference",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        conversion_reference: Option<PriceReferenceId>,
    },
    #[serde(rename = "future")]
    Future {
        #[serde(rename = "underlying")]
        underlying: DerivativeUnderlying,
        #[serde(rename = "price_quote")]
        price_quote: AssetId,
        #[serde(rename = "pnl_model")]
        pnl_model: DerivativePnlModel,
        #[serde(rename = "contract_size")]
        contract_size: ContractSize,
        #[serde(rename = "expiry")]
        expiry: Expiry,
        #[serde(rename = "first_notice_at")]
        first_notice_at: Option<UnixMillis>,
        #[serde(rename = "delivery")]
        delivery: DeliveryMode,
        #[serde(
            rename = "conversion_reference",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        conversion_reference: Option<PriceReferenceId>,
    },
    #[serde(rename = "option")]
    Option {
        #[serde(rename = "underlying")]
        underlying: DerivativeUnderlying,
        #[serde(rename = "price_quote")]
        price_quote: AssetId,
        #[serde(rename = "right")]
        right: OptionRight,
        #[serde(rename = "strike")]
        strike: StrikePrice,
        #[serde(rename = "expiry")]
        expiry: Expiry,
        #[serde(rename = "exercise")]
        exercise: ExerciseStyle,
        #[serde(rename = "contract_size")]
        contract_size: ContractSize,
        #[serde(rename = "delivery")]
        delivery: DeliveryMode,
        #[serde(
            rename = "conversion_reference",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        conversion_reference: Option<PriceReferenceId>,
    },
    #[serde(rename = "outcome")]
    Outcome {
        #[serde(rename = "group")]
        group: OutcomeGroupId,
        #[serde(rename = "member")]
        member: OutcomeMemberId,
        #[serde(rename = "side")]
        side: OutcomeSideId,
    },
}

impl From<&InstrumentTerms> for InstrumentWireDto {
    fn from(terms: &InstrumentTerms) -> Self {
        match terms {
            InstrumentTerms::Spot(terms) => Self::Spot {
                base: terms.base.clone(),
                quote: terms.quote.clone(),
            },
            InstrumentTerms::Perpetual(terms) => Self::Perpetual {
                underlying: terms.underlying.clone(),
                price_quote: terms.price_quote.clone(),
                settlement_asset: terms.settlement_asset.clone(),
                contract_size: terms.contract_size.clone(),
                pnl_model: terms.pnl_model,
                conversion_reference: terms.conversion_reference,
            },
            InstrumentTerms::Future(terms) => Self::Future {
                underlying: terms.underlying.clone(),
                price_quote: terms.price_quote.clone(),
                pnl_model: terms.pnl_model,
                contract_size: terms.contract_size.clone(),
                expiry: terms.expiry,
                first_notice_at: terms.first_notice_at,
                delivery: terms.delivery.clone(),
                conversion_reference: terms.delivery.cash_conversion_reference().cloned(),
            },
            InstrumentTerms::Option(terms) => Self::Option {
                underlying: terms.underlying.clone(),
                price_quote: terms.price_quote.clone(),
                right: terms.right,
                strike: terms.strike.clone(),
                expiry: terms.expiry,
                exercise: terms.exercise.clone(),
                contract_size: terms.contract_size.clone(),
                delivery: terms.delivery.clone(),
                conversion_reference: terms.delivery.cash_conversion_reference().cloned(),
            },
            InstrumentTerms::Outcome(terms) => Self::Outcome {
                group: terms.group,
                member: terms.member,
                side: terms.side,
            },
        }
    }
}

impl TryFrom<InstrumentWireDto> for InstrumentTerms {
    type Error = ModelError;

    fn try_from(wire: InstrumentWireDto) -> Result<Self, Self::Error> {
        Ok(match wire {
            InstrumentWireDto::Spot { base, quote } => Self::Spot(SpotTerms::new(base, quote)?),
            InstrumentWireDto::Perpetual {
                underlying,
                price_quote,
                settlement_asset,
                contract_size,
                pnl_model,
                conversion_reference,
            } => Self::Perpetual(PerpetualTerms::with_conversion_reference(
                underlying,
                price_quote,
                settlement_asset,
                contract_size,
                pnl_model,
                conversion_reference,
            )?),
            InstrumentWireDto::Future {
                underlying,
                price_quote,
                pnl_model,
                contract_size,
                expiry,
                first_notice_at,
                delivery,
                conversion_reference,
            } => Self::Future(FutureTerms::with_conversion_reference(
                underlying,
                price_quote,
                pnl_model,
                contract_size,
                expiry,
                first_notice_at,
                delivery,
                conversion_reference,
            )?),
            InstrumentWireDto::Option {
                underlying,
                price_quote,
                right,
                strike,
                expiry,
                exercise,
                contract_size,
                delivery,
                conversion_reference,
            } => Self::Option(OptionTerms::with_conversion_reference(
                underlying,
                price_quote,
                right,
                strike,
                expiry,
                exercise,
                contract_size,
                delivery,
                conversion_reference,
            )?),
            InstrumentWireDto::Outcome {
                group,
                member,
                side,
            } => Self::Outcome(OutcomeTerms::new(group, member, side)),
        })
    }
}
pub(crate) fn expected_price_basis(
    underlying: &DerivativeUnderlying,
    contract_size: &ContractSize,
) -> PriceBasis {
    if let Some(index) = contract_size.index() {
        return PriceBasis::PerIndexPoint { index: *index };
    }
    match underlying.reference() {
        UnderlyingRef::Index(index) => PriceBasis::PerIndexPoint { index: *index },
        UnderlyingRef::Asset(_) | UnderlyingRef::External(_) => PriceBasis::PerAssetUnit,
    }
}

pub(crate) fn validate_conversion_direction(
    conversion: &PriceReferenceTerms,
    settlement_asset: &AssetId,
    price_quote: Option<&AssetId>,
) -> Result<(), ModelError> {
    if conversion.underlying != UnderlyingRef::Asset(settlement_asset.clone()) {
        return Err(ModelError::ConversionDirectionMismatch {
            direction: ConversionDirection::Underlying,
        });
    }
    if let Some(price_quote) = price_quote
        && &conversion.quote_asset != price_quote
    {
        return Err(ModelError::ConversionDirectionMismatch {
            direction: ConversionDirection::Quote,
        });
    }
    if conversion.basis != PriceBasis::PerAssetUnit {
        return Err(ModelError::ConversionDirectionMismatch {
            direction: ConversionDirection::Basis,
        });
    }
    Ok(())
}

pub(crate) enum ConversionSource<'a> {
    Registry {
        carrier: Option<PriceReferenceId>,
        terms: Option<&'a PriceReferenceTerms>,
    },
    Identity {
        carrier: Option<PriceReferenceId>,
        mirror: Option<PriceReferenceId>,
    },
}

fn resolve_conversion(
    convertible: bool,
    source: ConversionSource<'_>,
    settlement_asset: &AssetId,
    price_quote: &AssetId,
) -> Result<Option<PriceReferenceId>, ModelError> {
    let (carrier, terms, mirror, holds_terms) = match source {
        ConversionSource::Registry { carrier, terms } => (carrier, terms, None, true),
        ConversionSource::Identity { carrier, mirror } => (carrier, None, mirror, false),
    };
    if !convertible {
        return if carrier.is_some() || terms.is_some() || mirror.is_some() {
            Err(ModelError::UnexpectedConversionReference)
        } else {
            Ok(None)
        };
    }
    if let Some(mirror) = &mirror {
        match &carrier {
            Some(carrier) if carrier != mirror => {
                return Err(ModelError::ConversionReferenceMismatch);
            }
            Some(_) => {}
            None => return Err(ModelError::RequiredConversionReference),
        }
    }
    let Some(carrier) = carrier else {
        return Err(ModelError::RequiredConversionReference);
    };
    if holds_terms {
        let Some(terms) = terms else {
            return Err(ModelError::RequiredConversionReference);
        };
        if terms.reference_id()? != carrier {
            return Err(ModelError::ConversionReferenceMismatch);
        }
        validate_conversion_direction(terms, settlement_asset, Some(price_quote))?;
    }
    Ok(Some(carrier))
}

pub(crate) fn convertible(
    is_physical: bool,
    settlement_asset: &AssetId,
    price_quote: &AssetId,
) -> bool {
    !is_physical && settlement_asset != price_quote
}

fn validate_contract_unit(
    underlying: &DerivativeUnderlying,
    price_quote: &AssetId,
    contract_size: &ContractSize,
) -> Result<(), ModelError> {
    match underlying.reference() {
        UnderlyingRef::Index(index) => {
            let Some(contract_index) = contract_size.index() else {
                return Err(ModelError::IndexMismatch {
                    field: IndexField::ContractUnit,
                });
            };
            if index != contract_index {
                return Err(ModelError::IndexMismatch {
                    field: IndexField::ContractUnitIndex,
                });
            }
            if contract_size.quote_asset() != Some(price_quote) {
                return Err(ModelError::IndexMismatch {
                    field: IndexField::ContractUnitQuoteAsset,
                });
            }
            Ok(())
        }
        UnderlyingRef::Asset(_) | UnderlyingRef::External(_) => {
            if contract_size.index().is_some() {
                return Err(ModelError::QuantityUnitMismatch {
                    field: "contract_unit",
                    expected: QuantityUnitExpectation::TradedQuantity,
                });
            }
            Ok(())
        }
    }
}

fn validate_pnl_underlying(
    underlying: &DerivativeUnderlying,
    pnl_model: DerivativePnlModel,
) -> Result<(), ModelError> {
    if pnl_model == DerivativePnlModel::Inverse && underlying.reference().asset().is_none() {
        return Err(ModelError::IncompatiblePnlModel);
    }
    Ok(())
}

fn validate_derivative_terms(
    underlying: &DerivativeUnderlying,
    price_quote: &AssetId,
    settlement_asset: &AssetId,
    contract_size: &ContractSize,
    pnl_model: DerivativePnlModel,
    delivery: Option<&DeliveryMode>,
) -> Result<(), ModelError> {
    validate_pnl_underlying(underlying, pnl_model)?;
    validate_contract_unit(underlying, price_quote, contract_size)?;
    validate_settlement_terms(
        underlying,
        price_quote,
        settlement_asset,
        contract_size,
        pnl_model,
        delivery,
    )?;
    if let Some(delivery) = delivery
        && delivery.is_physical()
    {
        validate_future_deliveries(underlying, delivery, contract_size)?;
    }
    Ok(())
}

fn validate_settlement_terms(
    underlying: &DerivativeUnderlying,
    price_quote: &AssetId,
    settlement_asset: &AssetId,
    contract_size: &ContractSize,
    pnl_model: DerivativePnlModel,
    delivery: Option<&DeliveryMode>,
) -> Result<(), ModelError> {
    match pnl_model {
        DerivativePnlModel::Linear => {
            if settlement_asset != price_quote {
                return Err(ModelError::IncompatibleDeliveryMode);
            }
        }
        DerivativePnlModel::Inverse => {
            if let Some(delivery) = delivery
                && delivery.is_physical()
            {
                return Err(ModelError::InverseRequiresCashDelivery);
            }
            let Some(underlying_asset) = underlying.reference().asset() else {
                return Err(ModelError::IncompatiblePnlModel);
            };
            let Some(denomination) = contract_size.denomination() else {
                return Err(ModelError::ContractSizeWithoutAsset);
            };
            if denomination != price_quote {
                return Err(ModelError::InverseContractSizeMismatch);
            }
            if settlement_asset != underlying_asset {
                return Err(ModelError::InverseSettlementMismatch);
            }
        }
        DerivativePnlModel::Quanto => {
            if settlement_asset == price_quote {
                return Err(ModelError::IncompatibleDeliveryMode);
            }
        }
    }
    if delivery.is_none_or(|delivery| !delivery.is_physical())
        && !settlement_asset.is_settlement_asset()
    {
        return Err(ModelError::UnsupportedSettlementAsset {
            asset: settlement_asset.clone(),
            role: SettlementAssetRole::CashDelivery,
        });
    }
    Ok(())
}

fn delivered_asset(underlying: &DerivativeUnderlying) -> Result<&AssetId, ModelError> {
    if let Some(asset) = underlying.reference().asset() {
        return Ok(asset);
    }
    Err(match underlying.reference() {
        UnderlyingRef::Index(_) => ModelError::IndexUnderlyingRequiresCashDelivery,
        UnderlyingRef::Asset(_) | UnderlyingRef::External(_) => ModelError::UnderlyingWithoutAsset,
    })
}

fn validate_future_deliveries(
    underlying: &DerivativeUnderlying,
    delivery: &DeliveryMode,
    contract_size: &ContractSize,
) -> Result<(), ModelError> {
    let underlying_asset = delivered_asset(underlying)?;
    let quantity = contract_size.quantity();
    let matching = delivery
        .deliveries()
        .iter()
        .any(|leg| leg.asset() == underlying_asset && leg.quantity() == quantity);
    if matching {
        Ok(())
    } else {
        Err(ModelError::DeliveryLegMismatch)
    }
}

fn validate_option_deliveries(
    underlying: &DerivativeUnderlying,
    delivery: &DeliveryMode,
    contract_size: &ContractSize,
    right: OptionRight,
) -> Result<(), ModelError> {
    let underlying_asset = delivered_asset(underlying)?;
    let Some(denomination) = contract_size.denomination() else {
        return Err(ModelError::ContractSizeWithoutAsset);
    };
    if underlying_asset != denomination {
        return Err(ModelError::DeliveryLegMismatch);
    }
    let expected = match right {
        OptionRight::Call => DeliveryDirection::Receive,
        OptionRight::Put => DeliveryDirection::Deliver,
    };
    let quantity = contract_size.quantity();
    let matching = delivery.deliveries().iter().any(|leg| {
        leg.asset() == underlying_asset && leg.quantity() == quantity && leg.direction() == expected
    });
    if matching {
        Ok(())
    } else {
        Err(ModelError::DeliveryLegMismatch)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        asset::{
            AssetId, CommodityId, CommodityOrigin, CryptoTokenId, CryptoTokenOrigin, CurrencyId,
            IndexId, Iso4217Code, TokenReference, UnderlyingRef,
        },
        decimal::{Price, Quantity, SignedQuantity},
        error::{
            ConversionDirection, IndexField, ModelError, QuantityUnitExpectation,
            SettlementAssetRole,
        },
        identifier::{CanonicalTerms, IdentifierNamespace, IdentifierValue, TokenIssuerId},
        instrument::{
            FutureTerms, Instrument, InstrumentKind, InstrumentTerms, OptionTerms, OutcomeTerms,
            PerpetualTerms, SpotTerms,
        },
        listing::venue::PriceBasis,
        market::account::AssetAmount,
        outcome::{OutcomeGroupId, OutcomeMemberId, OutcomeSideId},
        reference::{
            PriceReferenceId, PriceReferenceTerms, ReferenceAggregation, ReferenceObservation,
            ReferenceObservationKind, ReferenceSessionConvention, ReferenceSettlementConvention,
        },
        settlement::{
            ContractSize, ContractUnit, DeliveryDirection, DeliveryLeg, DeliveryMode,
            DerivativePnlModel, ExerciseStyle, Expiry, OptionRight, OptionSettlementObservation,
            StrikePrice,
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

    fn index() -> IndexId {
        IndexId::new(
            IdentifierNamespace::new("sp").expect("namespace should be valid"),
            IdentifierValue::new("500").expect("code should be valid"),
        )
    }

    fn price_terms(
        underlying: UnderlyingRef,
        quote: AssetId,
        basis: PriceBasis,
    ) -> PriceReferenceTerms {
        PriceReferenceTerms {
            underlying,
            quote_asset: quote,
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

    fn reference(underlying: UnderlyingRef, quote: AssetId, basis: PriceBasis) -> PriceReferenceId {
        price_terms(underlying, quote, basis)
            .reference_id()
            .expect("reference should be valid")
    }

    fn underlying(code: &str) -> crate::asset::DerivativeUnderlying {
        crate::asset::DerivativeUnderlying {
            reference: UnderlyingRef::Asset(venue_token(code)),
            price_reference: reference(
                UnderlyingRef::Asset(venue_token(code)),
                usd(),
                PriceBasis::PerAssetUnit,
            ),
        }
    }

    fn contract_size(code: &str, quantity: u8) -> ContractSize {
        ContractSize::new(
            ContractUnit::asset_quantity(Quantity::from(quantity), venue_token(code))
                .expect("unit should be valid"),
        )
        .expect("contract size should be valid")
    }

    fn quote_contract_size(quantity: u8) -> ContractSize {
        ContractSize::new(
            ContractUnit::asset_quantity(Quantity::from(quantity), usd())
                .expect("unit should be valid"),
        )
        .expect("contract size should be valid")
    }

    fn expiry() -> Expiry {
        Expiry::new(2_000, Some(1_800), Some(2_100)).expect("expiry should be valid")
    }

    fn index_underlying() -> crate::asset::DerivativeUnderlying {
        crate::asset::DerivativeUnderlying {
            reference: UnderlyingRef::Index(index()),
            price_reference: reference(
                UnderlyingRef::Index(index()),
                usd(),
                PriceBasis::PerIndexPoint { index: index() },
            ),
        }
    }

    fn index_contract_size() -> ContractSize {
        ContractSize::new(
            ContractUnit::quote_notional_per_index_point(Quantity::from(50_u8), usd(), index())
                .expect("unit should be valid"),
        )
        .expect("contract size should be valid")
    }

    fn external_underlying() -> crate::asset::DerivativeUnderlying {
        let external = crate::reference::ExternalReferenceTerms {
            issuer: IdentifierNamespace::new("hyperliquid").expect("issuer should be valid"),
            reference: IdentifierValue::new("oracle-1").expect("reference should be valid"),
            quote_asset: usd(),
            basis: PriceBasis::PerAssetUnit,
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
        };
        let id = external
            .reference_id()
            .expect("the external reference terms should be valid");
        crate::asset::DerivativeUnderlying {
            reference: UnderlyingRef::External(id),
            price_reference: reference(
                UnderlyingRef::External(id),
                usd(),
                PriceBasis::PerAssetUnit,
            ),
        }
    }

    fn physical(deliveries: Vec<DeliveryLeg>) -> DeliveryMode {
        DeliveryMode::physical(deliveries).expect("the legs should form a delivery")
    }

    fn quote_leg(quantity: u32, direction: DeliveryDirection) -> DeliveryLeg {
        DeliveryLeg::new(usd(), Quantity::from(quantity), direction)
            .expect("the quote leg should be valid")
    }

    fn asset_leg(quantity: u32, direction: DeliveryDirection) -> DeliveryLeg {
        DeliveryLeg::new(venue_token("BTC"), Quantity::from(quantity), direction)
            .expect("the asset leg should be valid")
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
        let delivery = DeliveryMode::cash(
            venue_token(code),
            reference(
                UnderlyingRef::Asset(venue_token("BTC")),
                price_quote.clone(),
                PriceBasis::PerAssetUnit,
            ),
            Some(&conversion),
        )
        .expect("delivery should be valid");
        (delivery, conversion)
    }

    fn cash_delivery() -> DeliveryMode {
        DeliveryMode::cash(
            usd(),
            reference(
                UnderlyingRef::Asset(venue_token("BTC")),
                usd(),
                PriceBasis::PerAssetUnit,
            ),
            None,
        )
        .expect("delivery should be valid")
    }

    fn cash_reference() -> PriceReferenceId {
        reference(
            UnderlyingRef::Asset(venue_token("BTC")),
            usd(),
            PriceBasis::PerAssetUnit,
        )
    }

    fn strike(value: u32) -> StrikePrice {
        StrikePrice::new(Price::from(value), usd(), PriceBasis::PerAssetUnit)
            .expect("strike should be valid")
    }

    fn terms_bytes<T: CanonicalTerms>(term: &T) -> Vec<u8> {
        let mut encoder = crate::identifier::CanonicalEncoder::new();
        encoder.write_terms(term);
        encoder.as_bytes().to_vec()
    }

    fn previous_cash_delivery_bytes(asset: &AssetId, reference: &PriceReferenceId) -> Vec<u8> {
        let mut encoder = crate::identifier::CanonicalEncoder::new();
        encoder.write_label("cash");
        encoder.write_terms(asset);
        encoder.write_terms(reference);
        encoder.as_bytes().to_vec()
    }

    fn physical_delivery_bytes(deliveries: &[DeliveryLeg]) -> Vec<u8> {
        let mut encoder = crate::identifier::CanonicalEncoder::new();
        encoder.write_label("physical");
        encoder.write_terms_slice(deliveries);
        encoder.as_bytes().to_vec()
    }

    fn cash_delivery_with_conversion_bytes(
        asset: &AssetId,
        reference: &PriceReferenceId,
        conversion: &PriceReferenceId,
    ) -> Vec<u8> {
        let mut bytes = previous_cash_delivery_bytes(asset, reference);
        let mut labeled = crate::identifier::CanonicalEncoder::new();
        labeled.write_label("conversion-reference");
        labeled.write_terms(conversion);
        bytes.extend_from_slice(labeled.as_bytes());
        bytes
    }

    fn decode_wire(value: serde_json::Value) -> Result<InstrumentTerms, ModelError> {
        let wire: super::InstrumentWireDto =
            serde_json::from_value(value).expect("the payload should decode into the wire shape");
        InstrumentTerms::try_from(wire)
    }

    fn wire_refusal(value: serde_json::Value) -> Result<ModelError, String> {
        let wire: super::InstrumentWireDto =
            serde_json::from_value(value).map_err(|error| error.to_string())?;
        Ok(InstrumentTerms::try_from(wire)
            .expect_err("the fixture describes no contract this model has"))
    }

    fn assert_public_decode_refuses(shape: &str, value: serde_json::Value) {
        assert!(
            serde_json::from_value::<Instrument>(value).is_err(),
            "{shape}: the public wire boundary must refuse the payload"
        );
    }

    fn assert_same_decision(
        shape: &str,
        wire: Result<InstrumentTerms, ModelError>,
        constructed: Result<InstrumentTerms, ModelError>,
    ) {
        match (wire, constructed) {
            (Ok(wire), Ok(constructed)) => {
                let wire = Instrument::new(wire).expect("the decoded terms build an instrument");
                let constructed = Instrument::new(constructed)
                    .expect("the constructed terms build an instrument");
                assert_eq!(
                    wire.id(),
                    constructed.id(),
                    "{shape}: both entries must describe one contract"
                );
            }
            (Err(wire), Err(constructed)) => assert_eq!(
                wire, constructed,
                "{shape}: both entries must refuse for the same reason"
            ),
            (wire, constructed) => {
                panic!("{shape}: the entries disagree, wire={wire:?} constructor={constructed:?}")
            }
        }
    }

    #[test]
    fn instrument_should_not_implement_ordering() {
        trait Unordered {}
        impl<T> Unordered for T {}

        fn assert_unordered<T: Unordered>() {}
        assert_unordered::<Instrument>();

        let spot = Instrument::new(InstrumentTerms::Spot(
            SpotTerms::new(venue_token("BTC"), usd()).expect("terms should be valid"),
        ))
        .expect("instrument should be valid");
        assert_eq!(spot.kind(), InstrumentKind::Spot);
    }

    #[test]
    fn spot_should_reject_identical_assets() {
        assert_eq!(
            SpotTerms::new(venue_token("BTC"), venue_token("BTC")),
            Err(ModelError::IdenticalSpotAssets)
        );
        assert!(SpotTerms::new(venue_token("BTC"), venue_token("USDT")).is_ok());
    }

    #[test]
    fn instrument_identity_should_follow_the_canonical_terms() {
        let spot = |quote: AssetId| {
            Instrument::new(InstrumentTerms::Spot(
                SpotTerms::new(venue_token("BTC"), quote).expect("terms should be valid"),
            ))
            .expect("instrument should be valid")
        };
        let first = spot(venue_token("USDT"));
        let second = spot(venue_token("USDT"));
        let other = spot(usd());

        assert_eq!(first.id(), second.id());
        assert_ne!(first.id(), other.id());
        assert!(first.id().as_str().starts_with("v1/"));
        assert_eq!(first.kind(), InstrumentKind::Spot);
    }

    #[test]
    fn linear_perpetual_should_settle_in_the_quote_without_a_conversion() {
        let terms = PerpetualTerms::new(
            underlying("BTC"),
            venue_token("USDT"),
            venue_token("USDT"),
            contract_size("BTC", 1),
            DerivativePnlModel::Linear,
            None,
        )
        .expect("terms should be valid");
        assert_eq!(terms.conversion_reference(), None);

        assert_eq!(
            PerpetualTerms::new(
                underlying("BTC"),
                venue_token("USDT"),
                venue_token("USDC"),
                contract_size("BTC", 1),
                DerivativePnlModel::Linear,
                None,
            ),
            Err(ModelError::IncompatibleDeliveryMode)
        );
        assert_eq!(
            PerpetualTerms::new(
                underlying("BTC"),
                venue_token("USDT"),
                venue_token("USDT"),
                contract_size("BTC", 1),
                DerivativePnlModel::Linear,
                Some(&conversion(venue_token("USDT"), &venue_token("USDT"))),
            ),
            Err(ModelError::UnexpectedConversionReference)
        );
    }

    #[test]
    fn quanto_perpetual_should_require_a_conversion_reference() {
        assert_eq!(
            PerpetualTerms::new(
                underlying("BTC"),
                venue_token("USDT"),
                venue_token("USDC"),
                contract_size("BTC", 1),
                DerivativePnlModel::Quanto,
                None,
            ),
            Err(ModelError::RequiredConversionReference)
        );

        let conversion = conversion(venue_token("USDC"), &venue_token("USDT"));
        let terms = PerpetualTerms::new(
            underlying("BTC"),
            venue_token("USDT"),
            venue_token("USDC"),
            contract_size("BTC", 1),
            DerivativePnlModel::Quanto,
            Some(&conversion),
        )
        .expect("terms should be valid");
        assert_eq!(
            terms.conversion_reference(),
            Some(
                &conversion
                    .reference_id()
                    .expect("reference should be valid")
            )
        );

        assert_eq!(
            PerpetualTerms::new(
                underlying("BTC"),
                venue_token("USDT"),
                venue_token("USDT"),
                contract_size("BTC", 1),
                DerivativePnlModel::Quanto,
                None,
            ),
            Err(ModelError::IncompatibleDeliveryMode)
        );
    }

    #[test]
    fn inverse_perpetual_should_settle_in_the_underlying_with_a_quote_denomination() {
        let conversion = conversion(venue_token("BTC"), &usd());
        let terms = PerpetualTerms::new(
            underlying("BTC"),
            usd(),
            venue_token("BTC"),
            quote_contract_size(100),
            DerivativePnlModel::Inverse,
            Some(&conversion),
        )
        .expect("an inverse perpetual settles in the underlying asset");
        assert_eq!(
            terms.conversion_reference(),
            Some(
                &conversion
                    .reference_id()
                    .expect("reference should be valid")
            )
        );
        assert_eq!(terms.settlement_asset(), &venue_token("BTC"));

        assert_eq!(
            PerpetualTerms::new(
                underlying("BTC"),
                usd(),
                venue_token("BTC"),
                contract_size("BTC", 1),
                DerivativePnlModel::Inverse,
                Some(&conversion),
            ),
            Err(ModelError::InverseContractSizeMismatch)
        );
        assert_eq!(
            PerpetualTerms::new(
                underlying("BTC"),
                usd(),
                usd(),
                quote_contract_size(100),
                DerivativePnlModel::Inverse,
                None,
            ),
            Err(ModelError::InverseSettlementMismatch)
        );
        assert_eq!(
            PerpetualTerms::new(
                crate::asset::DerivativeUnderlying {
                    reference: UnderlyingRef::Index(index()),
                    price_reference: reference(
                        UnderlyingRef::Index(index()),
                        usd(),
                        PriceBasis::PerIndexPoint { index: index() },
                    ),
                },
                usd(),
                venue_token("BTC"),
                quote_contract_size(100),
                DerivativePnlModel::Inverse,
                Some(&conversion),
            ),
            Err(ModelError::IncompatiblePnlModel)
        );
    }

    #[test]
    fn inverse_future_should_reject_a_physical_delivery() {
        let physical = DeliveryMode::physical(vec![
            DeliveryLeg::new(
                venue_token("BTC"),
                Quantity::from(1_u8),
                DeliveryDirection::Receive,
            )
            .expect("leg should be valid"),
        ])
        .expect("delivery should be valid");
        let conversion = conversion(venue_token("BTC"), &usd());

        assert_eq!(
            FutureTerms::new(
                underlying("BTC"),
                usd(),
                DerivativePnlModel::Inverse,
                quote_contract_size(100),
                expiry(),
                None,
                physical,
                None,
            ),
            Err(ModelError::InverseRequiresCashDelivery)
        );
        assert!(
            FutureTerms::new(
                underlying("BTC"),
                usd(),
                DerivativePnlModel::Inverse,
                quote_contract_size(100),
                expiry(),
                None,
                settlement("BTC", &usd()).0,
                Some(&conversion),
            )
            .is_ok()
        );
    }

    #[test]
    fn cash_future_should_carry_the_conversion_its_delivery_declared() {
        let conversion = conversion(venue_token("USDC"), &usd());
        let delivery = DeliveryMode::cash(
            venue_token("USDC"),
            reference(
                UnderlyingRef::Asset(venue_token("BTC")),
                usd(),
                PriceBasis::PerAssetUnit,
            ),
            Some(&conversion),
        )
        .expect("delivery should be valid");

        assert!(
            FutureTerms::new(
                underlying("BTC"),
                usd(),
                DerivativePnlModel::Quanto,
                contract_size("BTC", 5),
                expiry(),
                None,
                delivery.clone(),
                Some(&conversion),
            )
            .is_ok()
        );
        assert_eq!(
            FutureTerms::new(
                underlying("BTC"),
                usd(),
                DerivativePnlModel::Quanto,
                contract_size("BTC", 5),
                expiry(),
                None,
                delivery,
                None,
            ),
            Err(ModelError::RequiredConversionReference)
        );
    }

    #[test]
    fn physical_future_should_reject_a_conversion_reference() {
        let deliveries = DeliveryMode::physical(vec![
            DeliveryLeg::new(
                venue_token("BTC"),
                Quantity::from(1_u8),
                DeliveryDirection::Receive,
            )
            .expect("leg should be valid"),
        ])
        .expect("delivery should be valid");

        assert_eq!(
            FutureTerms::new(
                underlying("BTC"),
                usd(),
                DerivativePnlModel::Linear,
                contract_size("BTC", 1),
                expiry(),
                None,
                deliveries,
                Some(&conversion(venue_token("BTC"), &usd())),
            ),
            Err(ModelError::UnexpectedConversionReference)
        );
    }

    #[test]
    fn conversion_reference_direction_should_match_the_settlement_and_quote() {
        let direction_error = |direction| ModelError::ConversionDirectionMismatch { direction };
        let quanto = |conversion: &PriceReferenceTerms| {
            PerpetualTerms::new(
                underlying("BTC"),
                usd(),
                venue_token("USDC"),
                contract_size("BTC", 1),
                DerivativePnlModel::Quanto,
                Some(conversion),
            )
        };

        assert_eq!(
            quanto(&price_terms(
                UnderlyingRef::Asset(venue_token("USDC")),
                venue_token("USDT"),
                PriceBasis::PerAssetUnit,
            )),
            Err(direction_error(ConversionDirection::Quote))
        );
        assert_eq!(
            quanto(&price_terms(
                UnderlyingRef::Asset(venue_token("USDC")),
                usd(),
                PriceBasis::PerContract,
            )),
            Err(direction_error(ConversionDirection::Basis))
        );
        assert_eq!(
            DeliveryMode::cash(
                usd(),
                reference(
                    UnderlyingRef::Asset(venue_token("USDC")),
                    usd(),
                    PriceBasis::PerAssetUnit,
                ),
                Some(&price_terms(
                    UnderlyingRef::Asset(venue_token("ETH")),
                    usd(),
                    PriceBasis::PerAssetUnit,
                )),
            ),
            Err(direction_error(ConversionDirection::Underlying))
        );
        assert!(
            quanto(&conversion(venue_token("USDC"), &usd())).is_ok(),
            "a reference that tracks the settlement asset and quotes the price quote resolves"
        );
    }

    #[test]
    fn future_should_require_an_expiry_and_a_first_notice_inside_the_window() {
        assert!(
            FutureTerms::new(
                underlying("BTC"),
                usd(),
                DerivativePnlModel::Linear,
                contract_size("BTC", 5),
                expiry(),
                None,
                cash_delivery(),
                None,
            )
            .is_ok()
        );
        assert_eq!(
            FutureTerms::new(
                underlying("BTC"),
                usd(),
                DerivativePnlModel::Linear,
                contract_size("BTC", 5),
                expiry(),
                Some(1_900),
                cash_delivery(),
                None,
            ),
            Err(ModelError::InvalidTimeOrder {
                field: "first_notice_at",
                limit: "last_trading_at"
            })
        );
        assert!(
            FutureTerms::new(
                underlying("BTC"),
                usd(),
                DerivativePnlModel::Linear,
                contract_size("BTC", 5),
                expiry(),
                Some(1_700),
                cash_delivery(),
                None,
            )
            .is_ok()
        );
    }

    #[test]
    fn option_should_require_a_matching_strike_and_exercise() {
        let deliveries = DeliveryMode::physical(vec![
            DeliveryLeg::new(
                venue_token("BTC"),
                Quantity::from(5_u8),
                DeliveryDirection::Receive,
            )
            .expect("leg should be valid"),
            DeliveryLeg::new(usd(), Quantity::from(250_u8), DeliveryDirection::Deliver)
                .expect("leg should be valid"),
        ])
        .expect("delivery should be valid");

        assert_eq!(
            OptionTerms::new(
                underlying("BTC"),
                usd(),
                OptionRight::Call,
                StrikePrice::new(
                    Price::from(200_u32),
                    venue_token("USDT"),
                    PriceBasis::PerAssetUnit
                )
                .expect("strike should be valid"),
                expiry(),
                ExerciseStyle::American,
                contract_size("BTC", 5),
                deliveries.clone(),
                None,
            ),
            Err(ModelError::StrikeQuoteMismatch)
        );

        assert!(matches!(
            OptionTerms::new(
                underlying("BTC"),
                usd(),
                OptionRight::Call,
                StrikePrice::new(Price::from(200_u32), usd(), PriceBasis::PerAssetUnit)
                    .expect("strike should be valid"),
                expiry(),
                ExerciseStyle::Bermudan {
                    exercise_at: vec![2_500],
                },
                contract_size("BTC", 5),
                deliveries,
                None,
            ),
            Err(ModelError::InvalidTimeOrder { .. })
        ));
    }

    fn cross_asset_cash_option(right: OptionRight, strike: u32, quantity: u8) -> OptionTerms {
        let (delivery, conversion) = settlement("BTC", &usd());
        OptionTerms::new(
            underlying("BTC"),
            usd(),
            right,
            StrikePrice::new(Price::from(strike), usd(), PriceBasis::PerAssetUnit)
                .expect("strike should be valid"),
            expiry(),
            ExerciseStyle::European,
            contract_size("BTC", quantity),
            delivery,
            Some(&conversion),
        )
        .expect("terms should be valid")
    }

    fn cash_option(right: OptionRight, strike: u32, quantity: u8) -> OptionTerms {
        OptionTerms::new(
            underlying("BTC"),
            usd(),
            right,
            StrikePrice::new(Price::from(strike), usd(), PriceBasis::PerAssetUnit)
                .expect("strike should be valid"),
            expiry(),
            ExerciseStyle::European,
            contract_size("BTC", quantity),
            cash_delivery(),
            None,
        )
        .expect("terms should be valid")
    }

    #[test]
    fn cash_option_should_follow_the_conversion_rules_of_its_settlement_asset() {
        assert_eq!(
            cash_option(OptionRight::Call, 100, 1).conversion_reference(),
            None
        );

        let delivery = DeliveryMode::cash(
            venue_token("BTC"),
            reference(
                UnderlyingRef::Asset(venue_token("BTC")),
                usd(),
                PriceBasis::PerAssetUnit,
            ),
            None,
        )
        .expect("delivery should be valid");
        assert_eq!(
            OptionTerms::new(
                underlying("BTC"),
                usd(),
                OptionRight::Call,
                StrikePrice::new(Price::from(100_u32), usd(), PriceBasis::PerAssetUnit)
                    .expect("strike should be valid"),
                expiry(),
                ExerciseStyle::European,
                contract_size("BTC", 1),
                delivery,
                None,
            ),
            Err(ModelError::RequiredConversionReference)
        );

        let cross_asset = cross_asset_cash_option(OptionRight::Call, 100, 1);
        assert_eq!(
            cross_asset.conversion_reference(),
            Some(
                &conversion(venue_token("BTC"), &usd())
                    .reference_id()
                    .expect("reference")
            )
        );

        let quote_conversion = conversion(venue_token("USDT"), &usd());
        let delivery = DeliveryMode::cash(
            usd(),
            reference(
                UnderlyingRef::Asset(venue_token("BTC")),
                usd(),
                PriceBasis::PerAssetUnit,
            ),
            None,
        )
        .expect("delivery should be valid");
        assert_eq!(
            OptionTerms::new(
                underlying("BTC"),
                usd(),
                OptionRight::Call,
                StrikePrice::new(Price::from(100_u32), usd(), PriceBasis::PerAssetUnit)
                    .expect("strike should be valid"),
                expiry(),
                ExerciseStyle::European,
                contract_size("BTC", 1),
                delivery,
                Some(&quote_conversion),
            ),
            Err(ModelError::UnexpectedConversionReference)
        );
    }

    #[test]
    fn physical_option_should_reject_a_conversion_reference() {
        let deliveries = DeliveryMode::physical(vec![
            DeliveryLeg::new(
                venue_token("BTC"),
                Quantity::from(1_u8),
                DeliveryDirection::Receive,
            )
            .expect("leg should be valid"),
            DeliveryLeg::new(usd(), Quantity::from(100_u8), DeliveryDirection::Deliver)
                .expect("leg should be valid"),
        ])
        .expect("delivery should be valid");

        assert_eq!(
            OptionTerms::new(
                underlying("BTC"),
                usd(),
                OptionRight::Call,
                StrikePrice::new(Price::from(100_u32), usd(), PriceBasis::PerAssetUnit)
                    .expect("strike should be valid"),
                expiry(),
                ExerciseStyle::European,
                contract_size("BTC", 1),
                deliveries,
                Some(&conversion(venue_token("BTC"), &usd())),
            ),
            Err(ModelError::UnexpectedConversionReference)
        );
    }

    #[test]
    fn cash_settled_option_payoff_should_follow_the_call_and_put_formula() {
        let call = cash_option(OptionRight::Call, 100, 2);
        let payoff = call
            .cash_payoff(Price::from(120_u32), OptionSettlementObservation::Quote)
            .expect("payoff should resolve");
        assert_eq!(payoff.asset, usd());
        assert_eq!(
            payoff.value.inner(),
            (Quantity::from(40_u8)).inner() as i128
        );

        let put = cash_option(OptionRight::Put, 100, 2);
        let payoff = put
            .cash_payoff(Price::from(80_u32), OptionSettlementObservation::Quote)
            .expect("payoff should resolve");
        assert_eq!(payoff.asset, usd());
        assert_eq!(
            payoff.value.inner(),
            (Quantity::from(40_u8)).inner() as i128
        );
    }

    #[test]
    fn cash_settled_option_payoff_should_be_zero_when_out_of_the_money() {
        let call = cash_option(OptionRight::Call, 100, 2);
        assert_eq!(
            call.cash_payoff(Price::from(90_u32), OptionSettlementObservation::Quote)
                .expect("payoff should resolve")
                .value
                .inner(),
            0
        );

        let put = cash_option(OptionRight::Put, 100, 2);
        assert_eq!(
            put.cash_payoff(Price::from(110_u32), OptionSettlementObservation::Quote)
                .expect("payoff should resolve")
                .value
                .inner(),
            0
        );
    }

    #[test]
    fn cross_asset_option_payoff_should_require_the_converted_amount() {
        let option = cross_asset_cash_option(OptionRight::Call, 100, 2);
        let option_conversion = conversion(venue_token("BTC"), &usd());
        let reference_id = option_conversion
            .reference_id()
            .expect("reference should be valid");

        assert_eq!(
            option.cash_payoff(Price::from(120_u32), OptionSettlementObservation::Quote),
            Err(ModelError::RequiredConversionReference)
        );

        let other_conversion = conversion(venue_token("ETH"), &usd())
            .reference_id()
            .expect("reference should be valid");
        assert_eq!(
            option.cash_payoff(
                Price::from(120_u32),
                OptionSettlementObservation::converted(
                    other_conversion,
                    AssetAmount {
                        asset: venue_token("BTC"),
                        value: SignedQuantity::from_inner((Quantity::from(4_u8)).inner() as i128),
                    },
                ),
            ),
            Err(ModelError::ConversionReferenceMismatch)
        );

        assert_eq!(
            option.cash_payoff(
                Price::from(120_u32),
                OptionSettlementObservation::converted(
                    reference_id,
                    AssetAmount {
                        asset: usd(),
                        value: SignedQuantity::from_inner((Quantity::from(4_u8)).inner() as i128),
                    },
                ),
            ),
            Err(ModelError::SettlementAssetMismatch)
        );

        let payoff = option
            .cash_payoff(
                Price::from(120_u32),
                OptionSettlementObservation::converted(
                    reference_id,
                    AssetAmount {
                        asset: venue_token("BTC"),
                        value: SignedQuantity::from_inner((Quantity::from(4_u8)).inner() as i128),
                    },
                ),
            )
            .expect("a converted observation should resolve");
        assert_eq!(payoff.asset, venue_token("BTC"));
        assert_eq!(payoff.value.inner(), (Quantity::from(4_u8)).inner() as i128);
        assert_eq!(payoff.magnitude(), Quantity::from(4_u8));
    }

    #[test]
    fn inverse_unit_option_should_have_no_single_price_payoff() {
        let terms = OptionTerms::new(
            underlying("BTC"),
            usd(),
            OptionRight::Call,
            StrikePrice::new(Price::from(100_u32), usd(), PriceBasis::PerAssetUnit)
                .expect("strike should be valid"),
            expiry(),
            ExerciseStyle::European,
            quote_contract_size(100),
            cash_delivery(),
            None,
        )
        .expect("terms should be valid");

        assert_eq!(
            terms.cash_payoff(Price::from(120_u32), OptionSettlementObservation::Quote),
            Err(ModelError::PayoffUnitMismatch {
                underlying: venue_token("BTC"),
                denomination: usd(),
            })
        );
    }

    #[test]
    fn a_contract_without_an_asset_underlying_should_have_no_single_price_payoff() {
        let external = external_underlying();
        let option = OptionTerms::new(
            external.clone(),
            usd(),
            OptionRight::Call,
            strike(100),
            expiry(),
            ExerciseStyle::European,
            contract_size("BTC", 1),
            cash_delivery(),
            None,
        )
        .expect("an external reference can be tracked by a cash-settled contract");

        assert_eq!(
            option.cash_payoff(Price::from(120_u32), OptionSettlementObservation::Quote),
            Err(ModelError::UnderlyingWithoutAsset)
        );

        let index_point = OptionTerms::new(
            index_underlying(),
            usd(),
            OptionRight::Call,
            StrikePrice::new(
                Price::from(5_000_u32),
                usd(),
                PriceBasis::PerIndexPoint { index: index() },
            )
            .expect("strike should be valid"),
            expiry(),
            ExerciseStyle::European,
            index_contract_size(),
            DeliveryMode::cash(
                usd(),
                reference(
                    UnderlyingRef::Index(index()),
                    usd(),
                    PriceBasis::PerIndexPoint { index: index() },
                ),
                None,
            )
            .expect("delivery should be valid"),
            None,
        )
        .expect("the index option is a contract this model has");
        assert_eq!(
            index_point.cash_payoff(Price::from(5_100_u32), OptionSettlementObservation::Quote),
            Err(ModelError::IndexPointPayoffUnsupported)
        );

        let quote_only = OptionTerms::new(
            external,
            usd(),
            OptionRight::Call,
            strike(100),
            expiry(),
            ExerciseStyle::European,
            quote_contract_size(100),
            cash_delivery(),
            None,
        )
        .expect("terms should be valid");
        assert_eq!(
            quote_only.cash_payoff(Price::from(120_u32), OptionSettlementObservation::Quote),
            Err(ModelError::UnderlyingWithoutAsset)
        );
    }

    #[test]
    fn index_point_option_should_have_no_single_price_payoff() {
        let index_unit = ContractSize::new(
            ContractUnit::quote_notional_per_index_point(Quantity::from(50_u8), usd(), index())
                .expect("unit should be valid"),
        )
        .expect("contract size should be valid");
        let terms = OptionTerms::new(
            index_underlying(),
            usd(),
            OptionRight::Call,
            StrikePrice::new(
                Price::from(5_000_u32),
                usd(),
                PriceBasis::PerIndexPoint { index: index() },
            )
            .expect("strike should be valid"),
            expiry(),
            ExerciseStyle::European,
            index_unit,
            DeliveryMode::cash(
                usd(),
                reference(
                    UnderlyingRef::Index(index()),
                    usd(),
                    PriceBasis::PerIndexPoint { index: index() },
                ),
                None,
            )
            .expect("delivery should be valid"),
            None,
        )
        .expect("an index option that settles in a currency is a contract this model has");

        assert_eq!(
            terms.cash_payoff(Price::from(5_100_u32), OptionSettlementObservation::Quote),
            Err(ModelError::IndexPointPayoffUnsupported)
        );
    }

    #[test]
    fn physically_settled_option_should_have_no_cash_payoff() {
        let deliveries = DeliveryMode::physical(vec![
            DeliveryLeg::new(
                venue_token("BTC"),
                Quantity::from(2_u8),
                DeliveryDirection::Receive,
            )
            .expect("leg should be valid"),
            DeliveryLeg::new(usd(), Quantity::from(200_u8), DeliveryDirection::Deliver)
                .expect("leg should be valid"),
        ])
        .expect("delivery should be valid");
        let physical = OptionTerms::new(
            underlying("BTC"),
            usd(),
            OptionRight::Call,
            StrikePrice::new(Price::from(100_u32), usd(), PriceBasis::PerAssetUnit)
                .expect("strike should be valid"),
            expiry(),
            ExerciseStyle::European,
            contract_size("BTC", 2),
            deliveries,
            None,
        )
        .expect("terms should be valid");

        assert_eq!(
            physical.cash_payoff(Price::from(120_u32), OptionSettlementObservation::Quote),
            Err(ModelError::IncompatibleDeliveryMode)
        );
    }

    #[test]
    fn physical_option_delivery_should_follow_the_right() {
        let deliveries = |direction| {
            DeliveryMode::physical(vec![
                DeliveryLeg::new(venue_token("BTC"), Quantity::from(5_u8), direction)
                    .expect("leg should be valid"),
                DeliveryLeg::new(usd(), Quantity::from(250_u8), DeliveryDirection::Deliver)
                    .expect("leg should be valid"),
            ])
            .expect("delivery should be valid")
        };
        let strike = || {
            StrikePrice::new(Price::from(200_u32), usd(), PriceBasis::PerAssetUnit)
                .expect("strike should be valid")
        };

        assert!(
            OptionTerms::new(
                underlying("BTC"),
                usd(),
                OptionRight::Call,
                strike(),
                expiry(),
                ExerciseStyle::American,
                contract_size("BTC", 5),
                deliveries(DeliveryDirection::Receive),
                None,
            )
            .is_ok()
        );
        assert_eq!(
            OptionTerms::new(
                underlying("BTC"),
                usd(),
                OptionRight::Put,
                strike(),
                expiry(),
                ExerciseStyle::American,
                contract_size("BTC", 5),
                deliveries(DeliveryDirection::Receive),
                None,
            ),
            Err(ModelError::DeliveryLegMismatch)
        );
    }

    #[test]
    fn an_index_underlying_should_pair_with_an_index_point_unit_and_cash_settlement() {
        let index_unit = index_contract_size();
        let quote_settled = |basis| {
            DeliveryMode::cash(
                usd(),
                reference(UnderlyingRef::Index(index()), usd(), basis),
                None,
            )
            .expect("delivery should be valid")
        };
        let index_basis = || PriceBasis::PerIndexPoint { index: index() };

        assert_eq!(
            PerpetualTerms::new(
                index_underlying(),
                usd(),
                usd(),
                index_unit.clone(),
                DerivativePnlModel::Linear,
                None,
            )
            .expect("the index perpetual is a contract this model has")
            .conversion_reference(),
            None
        );
        assert!(
            FutureTerms::new(
                index_underlying(),
                usd(),
                DerivativePnlModel::Linear,
                index_unit.clone(),
                expiry(),
                None,
                quote_settled(index_basis()),
                None,
            )
            .is_ok(),
            "the index future"
        );
        assert!(
            OptionTerms::new(
                index_underlying(),
                usd(),
                OptionRight::Call,
                StrikePrice::new(Price::from(5_000_u32), usd(), index_basis())
                    .expect("strike should be valid"),
                expiry(),
                ExerciseStyle::European,
                index_unit.clone(),
                quote_settled(index_basis()),
                None,
            )
            .is_ok(),
            "the index option"
        );

        assert_eq!(
            FutureTerms::new(
                index_underlying(),
                usd(),
                DerivativePnlModel::Linear,
                contract_size("BTC", 5),
                expiry(),
                None,
                quote_settled(index_basis()),
                None,
            ),
            Err(ModelError::IndexMismatch {
                field: IndexField::ContractUnit
            })
        );
        assert_eq!(
            FutureTerms::new(
                index_underlying(),
                usd(),
                DerivativePnlModel::Linear,
                ContractSize::new(
                    ContractUnit::quote_notional_per_index_point(
                        Quantity::from(50_u8),
                        usd(),
                        IndexId::new(
                            IdentifierNamespace::new("sp").expect("namespace should be valid"),
                            IdentifierValue::new("100").expect("code should be valid"),
                        ),
                    )
                    .expect("unit should be valid"),
                )
                .expect("contract size should be valid"),
                expiry(),
                None,
                quote_settled(index_basis()),
                None,
            ),
            Err(ModelError::IndexMismatch {
                field: IndexField::ContractUnitIndex
            })
        );
        assert_eq!(
            FutureTerms::new(
                index_underlying(),
                usd(),
                DerivativePnlModel::Linear,
                ContractSize::new(
                    ContractUnit::quote_notional_per_index_point(
                        Quantity::from(50_u8),
                        venue_token("USDT"),
                        index(),
                    )
                    .expect("unit should be valid"),
                )
                .expect("contract size should be valid"),
                expiry(),
                None,
                quote_settled(index_basis()),
                None,
            ),
            Err(ModelError::IndexMismatch {
                field: IndexField::ContractUnitQuoteAsset
            })
        );
        assert_eq!(
            OptionTerms::new(
                index_underlying(),
                usd(),
                OptionRight::Call,
                StrikePrice::new(Price::from(5_000_u32), usd(), index_basis())
                    .expect("strike should be valid"),
                expiry(),
                ExerciseStyle::European,
                index_unit,
                DeliveryMode::physical(vec![
                    DeliveryLeg::new(usd(), Quantity::from(250_u8), DeliveryDirection::Deliver)
                        .expect("leg should be valid"),
                ])
                .expect("delivery should be valid"),
                None,
            ),
            Err(ModelError::IndexUnderlyingRequiresCashDelivery)
        );
    }

    #[test]
    fn an_asset_underlying_should_reject_an_index_point_unit() {
        let expected = Err(ModelError::QuantityUnitMismatch {
            field: "contract_unit",
            expected: QuantityUnitExpectation::TradedQuantity,
        });
        assert_eq!(
            PerpetualTerms::new(
                underlying("BTC"),
                usd(),
                usd(),
                index_contract_size(),
                DerivativePnlModel::Linear,
                None,
            ),
            expected,
            "the constructor refuses an index-point unit on an asset"
        );

        let payload = serde_json::json!({
            "kind": "perpetual",
            "spec": {
                "underlying": serde_json::to_value(underlying("BTC")).expect("underlying"),
                "price_quote": serde_json::to_value(usd()).expect("quote"),
                "settlement_asset": serde_json::to_value(usd()).expect("settlement asset"),
                "contract_size": serde_json::to_value(index_contract_size()).expect("size"),
                "pnl_model": serde_json::to_value(DerivativePnlModel::Linear).expect("pnl"),
            }
        });
        assert_eq!(
            decode_wire(payload.clone()),
            Err(ModelError::QuantityUnitMismatch {
                field: "contract_unit",
                expected: QuantityUnitExpectation::TradedQuantity,
            }),
            "the wire refuses it for the same reason"
        );
        assert_public_decode_refuses("asset underlying with an index-point unit", payload);
    }

    #[test]
    fn a_bermudan_option_should_carry_its_exercise_schedule() {
        let schedule = vec![1_900, 2_000];
        let exercise = ExerciseStyle::bermudan(schedule.clone(), &expiry())
            .expect("the schedule is inside the expiry");
        let terms = OptionTerms::new(
            underlying("BTC"),
            usd(),
            OptionRight::Put,
            strike(100),
            expiry(),
            exercise,
            contract_size("BTC", 2),
            cash_delivery(),
            None,
        )
        .expect("a Bermudan cash option is a contract this model has");
        assert_eq!(terms.exercise().exercise_at(), schedule.as_slice());
        assert_eq!(terms.exercise().label(), "bermudan");

        let instrument = Instrument::new(InstrumentTerms::Option(terms)).expect("instrument");
        let encoded = serde_json::to_value(&instrument).expect("the instrument should serialize");
        assert_eq!(
            encoded["spec"]["exercise"]["kind"],
            serde_json::json!("bermudan")
        );
        let decoded =
            serde_json::from_value::<Instrument>(encoded).expect("instrument should deserialize");
        assert_eq!(decoded, instrument);
        assert_eq!(decoded.id(), instrument.id());
    }

    #[test]
    fn a_physical_future_should_deliver_the_asset_it_tracks() {
        let future_of = |contract_size: ContractSize, delivery: DeliveryMode| {
            FutureTerms::new(
                underlying("BTC"),
                usd(),
                DerivativePnlModel::Linear,
                contract_size,
                expiry(),
                None,
                delivery,
                None,
            )
            .map(InstrumentTerms::Future)
        };
        for direction in [DeliveryDirection::Deliver, DeliveryDirection::Receive] {
            future_of(
                contract_size("BTC", 5),
                physical(vec![
                    asset_leg(5, direction),
                    quote_leg(500, direction.opposite()),
                ]),
            )
            .unwrap_or_else(|error| {
                panic!("{direction:?}: a physical future is a contract: {error}")
            });
        }
        assert_eq!(
            future_of(
                contract_size("BTC", 5),
                physical(vec![quote_leg(500, DeliveryDirection::Deliver)])
            ),
            Err(ModelError::DeliveryLegMismatch)
        );
        assert_eq!(
            future_of(
                contract_size("BTC", 5),
                physical(vec![
                    asset_leg(4, DeliveryDirection::Deliver),
                    quote_leg(500, DeliveryDirection::Receive)
                ])
            ),
            Err(ModelError::DeliveryLegMismatch)
        );

        let mut payload = serde_json::to_value(
            Instrument::new(
                future_of(
                    contract_size("BTC", 5),
                    physical(vec![
                        asset_leg(5, DeliveryDirection::Deliver),
                        quote_leg(500, DeliveryDirection::Receive),
                    ]),
                )
                .expect("the legal physical future is a contract this model has"),
            )
            .expect("the instrument builds"),
        )
        .expect("the instrument serializes");
        payload["spec"]["delivery"] =
            serde_json::to_value(physical(vec![quote_leg(500, DeliveryDirection::Deliver)]))
                .expect("the delivery serializes");
        assert_eq!(
            decode_wire(payload.clone()),
            Err(ModelError::DeliveryLegMismatch)
        );
        assert_public_decode_refuses("physical future without its tracked asset", payload);
    }

    #[test]
    fn a_physical_delivery_should_require_an_asset_underlying() {
        let legs = || physical(vec![asset_leg(5, DeliveryDirection::Deliver)]);

        assert_eq!(
            FutureTerms::new(
                index_underlying(),
                usd(),
                DerivativePnlModel::Linear,
                index_contract_size(),
                expiry(),
                None,
                physical(vec![quote_leg(250, DeliveryDirection::Deliver)]),
                None,
            ),
            Err(ModelError::IndexUnderlyingRequiresCashDelivery)
        );
        assert_eq!(
            FutureTerms::new(
                external_underlying(),
                usd(),
                DerivativePnlModel::Linear,
                contract_size("BTC", 5),
                expiry(),
                None,
                legs(),
                None,
            ),
            Err(ModelError::UnderlyingWithoutAsset)
        );
        assert_eq!(
            OptionTerms::new(
                external_underlying(),
                usd(),
                OptionRight::Call,
                strike(100),
                expiry(),
                ExerciseStyle::European,
                contract_size("BTC", 5),
                legs(),
                None,
            ),
            Err(ModelError::UnderlyingWithoutAsset)
        );

        let payload = serde_json::json!({
            "kind": "future",
            "spec": {
                "underlying": serde_json::to_value(index_underlying()).expect("underlying"),
                "price_quote": serde_json::to_value(usd()).expect("quote"),
                "pnl_model": serde_json::to_value(DerivativePnlModel::Linear).expect("pnl"),
                "contract_size": serde_json::to_value(index_contract_size()).expect("size"),
                "expiry": serde_json::to_value(expiry()).expect("expiry"),
                "first_notice_at": serde_json::Value::Null,
                "delivery": serde_json::to_value(physical(vec![quote_leg(
                    250,
                    DeliveryDirection::Deliver
                )]))
                .expect("delivery"),
            }
        });
        assert_eq!(
            decode_wire(payload.clone()),
            Err(ModelError::IndexUnderlyingRequiresCashDelivery)
        );
        assert_public_decode_refuses("physical index future", payload);
    }

    #[test]
    fn a_cash_settlement_asset_should_be_judged_the_same_way_by_a_constructor_and_the_wire() {
        let gold_payload = AssetId::Commodity(
            CommodityId::new(CommodityOrigin::Iso {
                code: IdentifierValue::new("XAU").expect("XAU should be valid"),
            })
            .expect("commodity should be valid"),
        );
        assert!(
            !gold_payload.is_settlement_asset(),
            "a commodity cannot denominate a payoff"
        );
        let cash_in_gold = DeliveryMode::Cash {
            asset: gold_payload.clone(),
            reference: reference(
                UnderlyingRef::Asset(venue_token("BTC")),
                usd(),
                PriceBasis::PerAssetUnit,
            ),
            conversion_reference: None,
        };
        let expected = Err(ModelError::UnsupportedSettlementAsset {
            asset: gold_payload.clone(),
            role: SettlementAssetRole::CashDelivery,
        });
        assert_eq!(
            cash_in_gold.validate(),
            expected,
            "the delivery itself refuses the asset"
        );

        let future = FutureTerms::new(
            underlying("BTC"),
            usd(),
            DerivativePnlModel::Quanto,
            contract_size("BTC", 5),
            expiry(),
            None,
            cash_in_gold.clone(),
            None,
        );
        assert_eq!(
            future,
            Err(ModelError::UnsupportedSettlementAsset {
                asset: gold_payload.clone(),
                role: SettlementAssetRole::CashDelivery,
            })
        );
        let option = OptionTerms::new(
            underlying("BTC"),
            usd(),
            OptionRight::Call,
            strike(100),
            expiry(),
            ExerciseStyle::European,
            contract_size("BTC", 1),
            cash_in_gold,
            None,
        );
        assert_eq!(
            option,
            Err(ModelError::UnsupportedSettlementAsset {
                asset: gold_payload.clone(),
                role: SettlementAssetRole::CashDelivery,
            })
        );

        let mut wire = serde_json::to_value(
            Instrument::new(InstrumentTerms::Future(
                FutureTerms::new(
                    underlying("BTC"),
                    usd(),
                    DerivativePnlModel::Quanto,
                    contract_size("BTC", 5),
                    expiry(),
                    None,
                    DeliveryMode::cash(
                        venue_token("USDC"),
                        reference(
                            UnderlyingRef::Asset(venue_token("BTC")),
                            usd(),
                            PriceBasis::PerAssetUnit,
                        ),
                        Some(&conversion(venue_token("USDC"), &usd())),
                    )
                    .expect("the cross-asset delivery is buildable"),
                    Some(&conversion(venue_token("USDC"), &usd())),
                )
                .expect("the cross-asset quanto future is buildable"),
            ))
            .expect("the instrument is buildable"),
        )
        .expect("the instrument should serialize");
        wire["spec"]["delivery"]["mode"]["asset"] =
            serde_json::to_value(&gold_payload).expect("the asset serializes");
        assert_public_decode_refuses("cash settlement in a commodity", wire.clone());
        let refusal = serde_json::from_value::<Instrument>(wire)
            .expect_err("the wire refuses the commodity settlement")
            .to_string();
        assert!(
            refusal.contains("cannot denominate a settlement for a cash delivery"),
            "{refusal}"
        );
    }

    #[test]
    fn perpetual_wire_should_refuse_a_settlement_asset_it_cannot_be_valued_in() {
        let gold = AssetId::Commodity(
            CommodityId::new(CommodityOrigin::Iso {
                code: IdentifierValue::new("XAU").expect("XAU should be valid"),
            })
            .expect("commodity should be valid"),
        );
        assert!(
            !gold.is_settlement_asset(),
            "a commodity cannot denominate a payoff"
        );
        let gold_underlying = crate::asset::DerivativeUnderlying {
            reference: UnderlyingRef::Asset(gold.clone()),
            price_reference: reference(
                UnderlyingRef::Asset(gold.clone()),
                usd(),
                PriceBasis::PerAssetUnit,
            ),
        };
        let gold_conversion = conversion(gold.clone(), &usd());
        let expected: Result<PerpetualTerms, ModelError> =
            Err(ModelError::UnsupportedSettlementAsset {
                asset: gold.clone(),
                role: SettlementAssetRole::CashDelivery,
            });

        let arrangements = [
            (
                DerivativePnlModel::Linear,
                gold.clone(),
                contract_size("BTC", 1),
                None,
            ),
            (
                DerivativePnlModel::Quanto,
                usd(),
                contract_size("BTC", 1),
                Some(&gold_conversion),
            ),
            (
                DerivativePnlModel::Inverse,
                usd(),
                quote_contract_size(100),
                Some(&gold_conversion),
            ),
        ];
        for (pnl_model, price_quote, contract_size, conversion) in arrangements {
            assert_eq!(
                PerpetualTerms::new(
                    gold_underlying.clone(),
                    price_quote.clone(),
                    gold.clone(),
                    contract_size.clone(),
                    pnl_model,
                    conversion,
                ),
                expected,
                "{pnl_model:?}: the constructor refuses the settlement asset"
            );

            let payload = serde_json::json!({
                "kind": "perpetual",
                "spec": {
                    "underlying": serde_json::to_value(&gold_underlying).expect("underlying"),
                    "price_quote": serde_json::to_value(&price_quote).expect("quote"),
                    "settlement_asset": serde_json::to_value(&gold).expect("settlement asset"),
                    "contract_size": serde_json::to_value(&contract_size).expect("size"),
                    "pnl_model": serde_json::to_value(pnl_model).expect("pnl"),
                    "conversion_reference": conversion
                        .map(|terms| terms.reference_id().expect("reference").to_string()),
                }
            });
            assert_eq!(
                decode_wire(payload.clone()),
                expected.clone().map(InstrumentTerms::Perpetual),
                "{pnl_model:?}: the wire refuses it for the same reason"
            );
            assert_public_decode_refuses("perpetual settling in a commodity", payload);
        }
    }

    fn cross_asset_delivery() -> (DeliveryMode, PriceReferenceTerms) {
        let conversion = conversion(venue_token("BTC"), &usd());
        let delivery = DeliveryMode::cash(
            venue_token("BTC"),
            reference(
                UnderlyingRef::Asset(venue_token("BTC")),
                usd(),
                PriceBasis::PerAssetUnit,
            ),
            Some(&conversion),
        )
        .expect("the cross-asset delivery is buildable");
        (delivery, conversion)
    }

    fn future_and_option_of(
        delivery: DeliveryMode,
        conversion: Option<&PriceReferenceTerms>,
    ) -> (
        Result<FutureTerms, ModelError>,
        Result<OptionTerms, ModelError>,
    ) {
        let future = FutureTerms::new(
            underlying("BTC"),
            usd(),
            DerivativePnlModel::Quanto,
            contract_size("BTC", 5),
            expiry(),
            None,
            delivery.clone(),
            conversion,
        );
        let option = OptionTerms::new(
            underlying("BTC"),
            usd(),
            OptionRight::Call,
            strike(100),
            expiry(),
            ExerciseStyle::European,
            contract_size("BTC", 1),
            delivery,
            conversion,
        );
        (future, option)
    }

    #[test]
    fn a_declared_conversion_should_require_its_terms_from_every_family() {
        let (delivery, declared) = cross_asset_delivery();

        let (future, option) = future_and_option_of(delivery.clone(), None);
        assert_eq!(future, Err(ModelError::RequiredConversionReference));
        assert_eq!(option, Err(ModelError::RequiredConversionReference));

        let mut other = conversion(venue_token("BTC"), &usd());
        other.observation.kind = ReferenceObservationKind::Index;
        let (future, option) = future_and_option_of(delivery.clone(), Some(&other));
        assert_eq!(future, Err(ModelError::ConversionReferenceMismatch));
        assert_eq!(option, Err(ModelError::ConversionReferenceMismatch));

        let (future, option) = future_and_option_of(delivery, Some(&declared));
        assert!(future.is_ok(), "the quanto future accepts its own terms");
        let option = option.expect("the cross-asset option accepts its own terms");
        assert_eq!(
            option.conversion_reference(),
            Some(&declared.reference_id().expect("reference"))
        );
    }

    #[test]
    fn a_contract_that_performs_no_conversion_should_refuse_a_declared_one() {
        let quote_conversion = conversion(usd(), &usd());
        let quote_settled_with_conversion = DeliveryMode::cash(
            usd(),
            reference(
                UnderlyingRef::Asset(venue_token("BTC")),
                usd(),
                PriceBasis::PerAssetUnit,
            ),
            Some(&quote_conversion),
        )
        .expect("a delivery that declares a conversion is buildable");

        assert_eq!(
            FutureTerms::new(
                underlying("BTC"),
                usd(),
                DerivativePnlModel::Inverse,
                quote_contract_size(100),
                expiry(),
                None,
                quote_settled_with_conversion.clone(),
                None,
            ),
            Err(ModelError::UnexpectedConversionReference),
            "the settlement asset is the price quote, so the contract converts nothing"
        );
        assert_eq!(
            FutureTerms::new(
                underlying("BTC"),
                usd(),
                DerivativePnlModel::Inverse,
                quote_contract_size(100),
                expiry(),
                None,
                quote_settled_with_conversion.clone(),
                Some(&quote_conversion),
            ),
            Err(ModelError::UnexpectedConversionReference),
            "and a caller's terms cannot introduce one either"
        );

        let (future, option) = future_and_option_of(quote_settled_with_conversion, None);
        assert_eq!(future, Err(ModelError::UnexpectedConversionReference));
        assert_eq!(option, Err(ModelError::UnexpectedConversionReference));
    }

    #[test]
    fn outcome_terms_should_only_reference_resolved_identities() {
        let group = OutcomeGroupId::from_identity_value(&format!("v1/{}", "1".repeat(64)))
            .expect("group identity");
        let member = OutcomeMemberId::from_identity_value(&format!("v1/{}", "2".repeat(64)))
            .expect("member identity");
        let side = OutcomeSideId::from_identity_value(&format!("v1/{}", "3".repeat(64)))
            .expect("side identity");
        let instrument = Instrument::new(InstrumentTerms::Outcome(OutcomeTerms::new(
            group, member, side,
        )))
        .expect("instrument should be valid");

        assert_eq!(instrument.kind(), InstrumentKind::Outcome);
        let InstrumentTerms::Outcome(terms) = instrument.terms() else {
            panic!("fixture must be an outcome instrument")
        };
        assert_eq!(terms.group(), &group);
        assert_eq!(terms.member(), &member);
        assert_eq!(terms.side(), &side);
    }

    #[test]
    fn outcome_terms_should_round_trip_through_their_own_wire_shape() {
        let identity = |digit: &str| format!("v1/{}", digit.repeat(64));
        let terms = OutcomeTerms::new(
            OutcomeGroupId::from_identity_value(&identity("1")).expect("group identity"),
            OutcomeMemberId::from_identity_value(&identity("2")).expect("member identity"),
            OutcomeSideId::from_identity_value(&identity("3")).expect("side identity"),
        );
        let encoded = serde_json::to_value(&terms).expect("terms should serialize");
        assert_eq!(
            encoded,
            serde_json::json!({
                "group": terms.group().as_str(),
                "member": terms.member().as_str(),
                "side": terms.side().as_str(),
            }),
            "the outcome wire shape is exactly the three resolved identities"
        );
        assert_eq!(
            serde_json::from_value::<OutcomeTerms>(encoded).expect("the payload should decode"),
            terms
        );

        let unpublished = serde_json::json!({
            "group": "v1/not-a-digest",
            "member": terms.member().as_str(),
            "side": terms.side().as_str(),
        });
        let refusal = serde_json::from_value::<OutcomeTerms>(unpublished)
            .expect_err("an identity that was never published does not decode");
        assert!(
            refusal.to_string().contains("is not a canonical identity"),
            "{refusal}"
        );

        let mut missing_member = serde_json::to_value(&terms).expect("terms should serialize");
        missing_member
            .as_object_mut()
            .expect("the payload is an object")
            .remove("member");
        assert!(
            serde_json::from_value::<OutcomeTerms>(missing_member).is_err(),
            "all three identities are mandatory"
        );
    }

    #[test]
    fn outcome_terms_should_be_named_in_the_error_a_bad_document_produces() {
        for (document, expected) in [
            (
                "[]",
                "invalid length 0, expected struct OutcomeTerms with 3 elements",
            ),
            (
                "7",
                "invalid type: integer `7`, expected struct OutcomeTerms",
            ),
            (
                r#""outcome""#,
                r#"invalid type: string "outcome", expected struct OutcomeTerms"#,
            ),
            ("null", "invalid type: null, expected struct OutcomeTerms"),
            (
                "true",
                "invalid type: boolean `true`, expected struct OutcomeTerms",
            ),
        ] {
            let error = serde_json::from_str::<OutcomeTerms>(document)
                .expect_err("the document is not a set of outcome terms");
            let message = error
                .to_string()
                .split(" at line ")
                .next()
                .expect("serde_json renders a message")
                .to_owned();
            assert_eq!(message, expected, "{document}");
        }
    }

    #[test]
    fn every_product_family_should_round_trip_through_serde() {
        let (cross_asset_delivery, conversion) = settlement("BTC", &usd());
        let instruments = [
            Instrument::new(InstrumentTerms::Spot(
                SpotTerms::new(venue_token("BTC"), venue_token("USDT")).expect("terms"),
            ))
            .expect("instrument should be valid"),
            Instrument::new(InstrumentTerms::Perpetual(
                PerpetualTerms::new(
                    underlying("BTC"),
                    venue_token("USDT"),
                    venue_token("USDT"),
                    contract_size("BTC", 1),
                    DerivativePnlModel::Linear,
                    None,
                )
                .expect("terms should be valid"),
            ))
            .expect("instrument should be valid"),
            Instrument::new(InstrumentTerms::Perpetual(
                PerpetualTerms::new(
                    underlying("BTC"),
                    usd(),
                    venue_token("BTC"),
                    quote_contract_size(100),
                    DerivativePnlModel::Inverse,
                    Some(&conversion),
                )
                .expect("terms should be valid"),
            ))
            .expect("instrument should be valid"),
            Instrument::new(InstrumentTerms::Future(
                FutureTerms::new(
                    underlying("BTC"),
                    usd(),
                    DerivativePnlModel::Linear,
                    contract_size("BTC", 5),
                    expiry(),
                    Some(1_700),
                    cash_delivery(),
                    None,
                )
                .expect("terms should be valid"),
            ))
            .expect("instrument should be valid"),
            Instrument::new(InstrumentTerms::Option(
                OptionTerms::new(
                    underlying("BTC"),
                    usd(),
                    OptionRight::Call,
                    StrikePrice::new(Price::from(200_u32), usd(), PriceBasis::PerAssetUnit)
                        .expect("strike should be valid"),
                    expiry(),
                    ExerciseStyle::American,
                    contract_size("BTC", 5),
                    cross_asset_delivery,
                    Some(&conversion),
                )
                .expect("terms should be valid"),
            ))
            .expect("instrument should be valid"),
            Instrument::new(InstrumentTerms::Outcome(OutcomeTerms::new(
                OutcomeGroupId::from_identity_value(&format!("v1/{}", "1".repeat(64)))
                    .expect("group identity"),
                OutcomeMemberId::from_identity_value(&format!("v1/{}", "2".repeat(64)))
                    .expect("member identity"),
                OutcomeSideId::from_identity_value(&format!("v1/{}", "3".repeat(64)))
                    .expect("side identity"),
            )))
            .expect("instrument should be valid"),
        ];

        for instrument in instruments {
            let encoded = serde_json::to_string(&instrument).expect("instrument should serialize");
            let decoded = serde_json::from_str::<Instrument>(&encoded)
                .expect("instrument should deserialize");

            assert_eq!(decoded, instrument);
            assert_eq!(decoded.id(), instrument.id());
        }
    }

    #[test]
    fn quote_settled_perpetual_should_encode_the_previous_bytes() {
        let terms = PerpetualTerms::new(
            underlying("BTC"),
            usd(),
            usd(),
            contract_size("BTC", 1),
            DerivativePnlModel::Linear,
            None,
        )
        .expect("terms should be valid");
        let mut encoder = crate::identifier::CanonicalEncoder::new();
        terms.encode(&mut encoder);
        let mut expected = crate::identifier::CanonicalEncoder::new();
        expected.write_terms(&underlying("BTC"));
        expected.write_terms(&usd());
        expected.write_terms(&usd());
        expected.write_terms(&contract_size("BTC", 1));
        expected.write_terms(&DerivativePnlModel::Linear);

        assert_eq!(encoder.as_bytes(), expected.as_bytes());
        assert_eq!(encoder.as_bytes().len(), expected.as_bytes().len());
    }

    #[test]
    fn cross_asset_perpetual_should_encode_and_decode_its_conversion() {
        let conversion = conversion(venue_token("BTC"), &usd());
        let terms = PerpetualTerms::new(
            underlying("BTC"),
            usd(),
            venue_token("BTC"),
            quote_contract_size(100),
            DerivativePnlModel::Inverse,
            Some(&conversion),
        )
        .expect("terms should be valid");
        let expected_conversion = terms.conversion_reference().cloned();
        let mut encoder = crate::identifier::CanonicalEncoder::new();
        terms.encode(&mut encoder);

        let mut expected = terms_bytes(&underlying("BTC"));
        expected.extend_from_slice(&terms_bytes(&usd()));
        expected.extend_from_slice(&terms_bytes(&venue_token("BTC")));
        expected.extend_from_slice(&terms_bytes(&quote_contract_size(100)));
        expected.extend_from_slice(&terms_bytes(&DerivativePnlModel::Inverse));
        let mut labeled = crate::identifier::CanonicalEncoder::new();
        labeled.write_label("conversion-reference");
        labeled.write_terms(
            expected_conversion
                .as_ref()
                .expect("the cross-asset terms carry a conversion reference"),
        );
        expected.extend_from_slice(labeled.as_bytes());
        assert_eq!(
            encoder.as_bytes(),
            expected.as_slice(),
            "the conversion reference is exactly a marker and the identity it names"
        );

        let mut without_conversion = crate::identifier::CanonicalEncoder::new();
        PerpetualTerms::new(
            underlying("BTC"),
            usd(),
            usd(),
            quote_contract_size(100),
            DerivativePnlModel::Linear,
            None,
        )
        .expect("terms should be valid")
        .encode(&mut without_conversion);

        assert_ne!(
            encoder.as_bytes(),
            without_conversion.as_bytes(),
            "the conversion reference is part of the canonical bytes"
        );

        let instrument =
            Instrument::new(InstrumentTerms::Perpetual(terms)).expect("instrument should be valid");
        let encoded = serde_json::to_value(&instrument).expect("instrument should serialize");
        assert_eq!(
            encoded["spec"]["conversion_reference"],
            serde_json::to_value(
                expected_conversion.expect("the cross-asset terms carry a conversion reference")
            )
            .expect("the reference should serialize"),
            "a cross-asset perpetual must state the conversion identity it was built with"
        );
        let decoded =
            serde_json::from_value::<Instrument>(encoded).expect("instrument should deserialize");
        let InstrumentTerms::Perpetual(decoded_terms) = decoded.terms() else {
            panic!("fixture must be a perpetual")
        };

        assert_eq!(decoded, instrument);
        assert_eq!(decoded.id(), instrument.id());
        assert_eq!(
            decoded_terms.conversion_reference().cloned(),
            expected_conversion
        );
    }

    #[test]
    fn instrument_should_reject_a_malformed_wire_value_without_panicking() {
        let instrument = Instrument::new(InstrumentTerms::Perpetual(
            PerpetualTerms::new(
                underlying("BTC"),
                usd(),
                venue_token("BTC"),
                quote_contract_size(100),
                DerivativePnlModel::Inverse,
                Some(&conversion(venue_token("BTC"), &usd())),
            )
            .expect("terms should be valid"),
        ))
        .expect("instrument should be valid");
        let encoded = serde_json::to_value(&instrument).expect("instrument should serialize");

        let mut unexpected_conversion = encoded.clone();
        unexpected_conversion["spec"]["settlement_asset"] =
            serde_json::to_value(usd()).expect("asset should serialize");
        assert_eq!(
            decode_wire(unexpected_conversion.clone()),
            Err(ModelError::InverseSettlementMismatch),
            "the refusal is the typed rule the constructor states"
        );
        assert!(serde_json::from_value::<Instrument>(unexpected_conversion).is_err());

        let mut missing_conversion = encoded.clone();
        missing_conversion["spec"]
            .as_object_mut()
            .expect("spec is an object")
            .remove("conversion_reference");
        assert_eq!(
            decode_wire(missing_conversion.clone()),
            Err(ModelError::RequiredConversionReference),
            "a cross-asset contract has to state which reference it converts through"
        );
        assert!(serde_json::from_value::<Instrument>(missing_conversion).is_err());

        let mut unknown_kind = encoded;
        unknown_kind["kind"] = serde_json::Value::String("swap".into());
        assert!(serde_json::from_value::<Instrument>(unknown_kind).is_err());
        assert!(serde_json::from_str::<Instrument>(r#"{"kind":"perpetual"}"#).is_err());
    }

    #[test]
    fn every_family_should_refuse_a_malformed_payload_with_its_own_reason() {
        let spot = || {
            serde_json::to_value(
                Instrument::new(InstrumentTerms::Spot(
                    SpotTerms::new(venue_token("BTC"), usd()).expect("terms should be valid"),
                ))
                .expect("instrument should be valid"),
            )
            .expect("instrument should serialize")
        };
        let future = || {
            serde_json::to_value(
                Instrument::new(InstrumentTerms::Future(
                    FutureTerms::new(
                        underlying("BTC"),
                        usd(),
                        DerivativePnlModel::Linear,
                        contract_size("BTC", 5),
                        expiry(),
                        Some(1_700),
                        cash_delivery(),
                        None,
                    )
                    .expect("terms should be valid"),
                ))
                .expect("instrument should be valid"),
            )
            .expect("instrument should serialize")
        };
        let option = || {
            serde_json::to_value(
                Instrument::new(InstrumentTerms::Option(cash_option(
                    OptionRight::Call,
                    100,
                    1,
                )))
                .expect("instrument should be valid"),
            )
            .expect("instrument should serialize")
        };
        let outcome = || {
            let identity = |digit: &str| format!("v1/{}", digit.repeat(64));
            serde_json::to_value(
                Instrument::new(InstrumentTerms::Outcome(OutcomeTerms::new(
                    OutcomeGroupId::from_identity_value(&identity("1")).expect("group"),
                    OutcomeMemberId::from_identity_value(&identity("2")).expect("member"),
                    OutcomeSideId::from_identity_value(&identity("3")).expect("side"),
                )))
                .expect("instrument should be valid"),
            )
            .expect("instrument should serialize")
        };

        let mut identical = spot();
        identical["spec"]["quote"] = identical["spec"]["base"].clone();
        assert_eq!(
            wire_refusal(identical.clone()),
            Ok(ModelError::IdenticalSpotAssets)
        );
        assert_public_decode_refuses("spot against itself", identical);

        let mut late_notice = future();
        late_notice["spec"]["first_notice_at"] = serde_json::json!(1_900);
        assert_eq!(
            wire_refusal(late_notice.clone()),
            Ok(ModelError::InvalidTimeOrder {
                field: "first_notice_at",
                limit: "last_trading_at",
            })
        );
        assert_public_decode_refuses("future with a late first notice", late_notice);

        let mut inverted = future();
        inverted["spec"]["expiry"]["last_trading_at"] = serde_json::json!(2_100);
        let refusal = wire_refusal(inverted.clone()).expect_err("the expiry does not decode");
        assert!(
            refusal.contains("last_trading_at must not be later than expiration_at"),
            "{refusal}"
        );
        assert_public_decode_refuses("future with an inverted expiry", inverted);

        let mut foreign_strike = option();
        foreign_strike["spec"]["strike"]["quote_asset"] =
            serde_json::to_value(venue_token("USDT")).expect("asset should serialize");
        assert_eq!(
            wire_refusal(foreign_strike.clone()),
            Ok(ModelError::StrikeQuoteMismatch)
        );
        assert_public_decode_refuses("option with a foreign strike quote", foreign_strike);

        let mut late_schedule = option();
        late_schedule["spec"]["exercise"] = serde_json::json!({
            "kind": "bermudan",
            "style": { "exercise_at": [2_100, 2_200] },
        });
        assert_eq!(
            wire_refusal(late_schedule.clone()),
            Ok(ModelError::InvalidTimeOrder {
                field: "exercise_at",
                limit: "expiration_at",
            })
        );
        assert_public_decode_refuses("option exercising after its expiry", late_schedule);

        let mut unsorted_schedule = option();
        unsorted_schedule["spec"]["exercise"] = serde_json::json!({
            "kind": "bermudan",
            "style": { "exercise_at": [2_000, 1_900] },
        });
        let refusal = wire_refusal(unsorted_schedule.clone())
            .expect_err("the exercise schedule does not decode");
        assert!(
            refusal.contains("exercise_at` must be strictly increasing"),
            "{refusal}"
        );
        assert_public_decode_refuses(
            "option with an unsorted exercise schedule",
            unsorted_schedule,
        );

        let mut identity = outcome();
        identity["spec"]["group"] = serde_json::json!("v1/not-a-digest");
        let refusal = wire_refusal(identity.clone()).expect_err("the identity does not decode");
        assert!(refusal.contains("is not a canonical identity"), "{refusal}");
        assert_public_decode_refuses("outcome with an unpublished identity", identity);

        for (shape, mut payload) in [
            ("spot", spot()),
            ("future", future()),
            ("option", option()),
            ("outcome", outcome()),
        ] {
            payload["kind"] = serde_json::json!("swap");
            assert!(
                serde_json::from_value::<Instrument>(payload).is_err(),
                "{shape}: an unknown family is a decoding error, not a panic"
            );
        }

        for document in [
            r#"{"kind":"spot"}"#,
            r#"{"kind":"future","spec":{}}"#,
            r#"{"kind":"option","spec":{"underlying":null}}"#,
            r#"{"kind":"outcome","spec":{"group":"v1/"}}"#,
        ] {
            assert!(
                serde_json::from_str::<Instrument>(document).is_err(),
                "{document}: a truncated payload must not decode"
            );
        }
    }

    #[test]
    fn wire_shape_rules_of_a_cash_option_should_match_its_constructor() {
        let cross = conversion(venue_token("BTC"), &usd());
        let cross_id = cross.reference_id().expect("reference should be valid");
        let quote_conversion = conversion(usd(), &usd());
        let quote_id = quote_conversion
            .reference_id()
            .expect("reference should be valid");
        let quote_settled_instrument = Instrument::new(InstrumentTerms::Option(cash_option(
            OptionRight::Call,
            100,
            1,
        )))
        .expect("instrument should be valid");
        let cross_asset = cross_asset_cash_option(OptionRight::Call, 100, 1);
        let cross_asset_instrument =
            Instrument::new(InstrumentTerms::Option(cross_asset.clone())).expect("instrument");
        let quote_settled_delivery =
            || DeliveryMode::cash(usd(), cash_reference(), None).expect("delivery should be valid");
        let cross_asset_delivery = || {
            DeliveryMode::cash(venue_token("BTC"), cash_reference(), None)
                .expect("delivery should be valid")
        };
        let option_of = |delivery: DeliveryMode, conversion: Option<&PriceReferenceTerms>| {
            OptionTerms::new(
                underlying("BTC"),
                usd(),
                OptionRight::Call,
                strike(100),
                expiry(),
                ExerciseStyle::European,
                contract_size("BTC", 1),
                delivery,
                conversion,
            )
            .map(InstrumentTerms::Option)
        };

        let mut delivery_conversion = serde_json::to_value(&quote_settled_instrument)
            .expect("the instrument should serialize");
        delivery_conversion["spec"]["delivery"]["mode"]["conversion_reference"] =
            serde_json::json!(quote_id.as_str());
        assert_public_decode_refuses(
            "quote-settled cash option with a delivery conversion",
            delivery_conversion.clone(),
        );
        assert_same_decision(
            "quote-settled cash option with a delivery conversion",
            decode_wire(delivery_conversion.clone()),
            option_of(
                DeliveryMode::cash(usd(), cash_reference(), Some(&quote_conversion))
                    .expect("a delivery that declares a conversion is buildable"),
                None,
            ),
        );

        let mut mirrored_conversion = delivery_conversion.clone();
        mirrored_conversion["spec"]["conversion_reference"] = serde_json::json!(quote_id.as_str());
        assert_public_decode_refuses(
            "quote-settled cash option with both conversion fields",
            mirrored_conversion.clone(),
        );
        assert_same_decision(
            "quote-settled cash option with both conversion fields",
            decode_wire(mirrored_conversion),
            option_of(
                DeliveryMode::cash(usd(), cash_reference(), Some(&quote_conversion))
                    .expect("a delivery that declares a conversion is buildable"),
                Some(&quote_conversion),
            ),
        );

        let mut stripped =
            serde_json::to_value(&cross_asset_instrument).expect("the instrument should serialize");
        stripped["spec"]["conversion_reference"] = serde_json::Value::Null;
        stripped["spec"]["delivery"]["mode"]
            .as_object_mut()
            .expect("the delivery mode is an object")
            .remove("conversion_reference");
        assert_public_decode_refuses(
            "cross-asset cash option without a conversion",
            stripped.clone(),
        );
        assert_same_decision(
            "cross-asset cash option without a conversion",
            decode_wire(stripped.clone()),
            option_of(cross_asset_delivery(), None),
        );

        let mut mirror_only = stripped;
        mirror_only["spec"]["conversion_reference"] = serde_json::json!(cross_id.as_str());
        assert_public_decode_refuses(
            "cross-asset cash option mirrored without a delivery conversion",
            mirror_only.clone(),
        );
        assert_same_decision(
            "cross-asset cash option mirrored without a delivery conversion",
            decode_wire(mirror_only),
            option_of(cross_asset_delivery(), Some(&cross)),
        );
        assert_same_decision(
            "cross-asset cash option with its conversion",
            decode_wire(
                serde_json::to_value(&cross_asset_instrument)
                    .expect("the instrument should serialize"),
            ),
            option_of(
                DeliveryMode::cash(venue_token("BTC"), cash_reference(), Some(&cross))
                    .expect("the cross-asset delivery is buildable"),
                Some(&cross),
            ),
        );
        assert_eq!(
            decode_wire(serde_json::to_value(&cross_asset_instrument).expect("serialize"))
                .expect("the payload decodes"),
            InstrumentTerms::Option(cross_asset)
        );
        let _ = quote_settled_delivery;
    }

    #[test]
    fn wire_conversion_of_a_future_should_come_from_its_cash_delivery() {
        let cross = conversion(venue_token("BTC"), &usd());
        let cross_id = cross.reference_id().expect("reference should be valid");
        let mut other = conversion(venue_token("BTC"), &usd());
        other.observation.kind = ReferenceObservationKind::Index;
        let other_id = other.reference_id().expect("reference should be valid");

        let quanto = || {
            FutureTerms::new(
                underlying("BTC"),
                usd(),
                DerivativePnlModel::Quanto,
                contract_size("BTC", 5),
                expiry(),
                None,
                DeliveryMode::cash(venue_token("BTC"), cash_reference(), Some(&cross))
                    .expect("the cross-asset delivery is buildable"),
                Some(&cross),
            )
            .expect("the honest quanto future is valid")
        };
        let instrument = Instrument::new(InstrumentTerms::Future(quanto())).expect("instrument");
        let encoded = serde_json::to_value(&instrument).expect("the instrument should serialize");
        let future_of = |delivery: DeliveryMode, conversion: Option<&PriceReferenceTerms>| {
            FutureTerms::new(
                underlying("BTC"),
                usd(),
                DerivativePnlModel::Quanto,
                contract_size("BTC", 5),
                expiry(),
                None,
                delivery,
                conversion,
            )
            .map(InstrumentTerms::Future)
        };
        let delivery_without_conversion = || {
            DeliveryMode::cash(venue_token("BTC"), cash_reference(), None)
                .expect("delivery should be valid")
        };

        let mut mirrored = encoded.clone();
        mirrored["spec"]["delivery"]["mode"]["conversion_reference"] = serde_json::Value::Null;
        mirrored["spec"]["conversion_reference"] = serde_json::json!(other_id.as_str());
        assert_public_decode_refuses(
            "quanto future mirrored without a delivery conversion",
            mirrored.clone(),
        );
        assert_same_decision(
            "quanto future mirrored without a delivery conversion",
            decode_wire(mirrored),
            future_of(delivery_without_conversion(), Some(&other)),
        );

        let mut contradicting = encoded.clone();
        contradicting["spec"]["conversion_reference"] = serde_json::json!(other_id.as_str());
        assert_public_decode_refuses(
            "quanto future with two different conversion identities",
            contradicting.clone(),
        );
        assert_same_decision(
            "quanto future with two different conversion identities",
            decode_wire(contradicting),
            future_of(
                DeliveryMode::cash(venue_token("BTC"), cash_reference(), Some(&cross))
                    .expect("the cross-asset delivery is buildable"),
                Some(&other),
            ),
        );

        let mut absent = encoded.clone();
        absent["spec"]["conversion_reference"] = serde_json::Value::Null;
        absent["spec"]["delivery"]["mode"]
            .as_object_mut()
            .expect("the delivery mode is an object")
            .remove("conversion_reference");
        assert_public_decode_refuses("quanto future without any conversion", absent.clone());
        assert_same_decision(
            "quanto future without any conversion",
            decode_wire(absent),
            future_of(delivery_without_conversion(), None),
        );

        let mut delivery_only = encoded;
        delivery_only["spec"]["conversion_reference"] = serde_json::Value::Null;
        let decoded = decode_wire(delivery_only.clone()).expect("the delivery owns the identity");
        assert_eq!(decoded, InstrumentTerms::Future(quanto()));
        let InstrumentTerms::Future(terms) = decoded else {
            panic!("fixture must be a future")
        };
        assert_eq!(
            terms.delivery().cash_conversion_reference(),
            Some(&cross_id)
        );
        assert_eq!(
            Instrument::new(InstrumentTerms::Future(terms))
                .expect("instrument")
                .id(),
            instrument.id()
        );
    }

    #[test]
    fn wire_conversion_of_an_option_should_come_from_its_cash_delivery() {
        let cross = conversion(venue_token("BTC"), &usd());
        let cross_id = cross.reference_id().expect("reference should be valid");
        let option = cross_asset_cash_option(OptionRight::Call, 100, 1);
        let instrument = Instrument::new(InstrumentTerms::Option(option)).expect("instrument");

        let mirrored = serde_json::to_value(&instrument).expect("the instrument should serialize");
        let mut omitted = mirrored.clone();
        omitted["spec"]
            .as_object_mut()
            .expect("the spec is an object")
            .remove("conversion_reference");

        for (shape, payload) in [
            ("mirror present", mirrored.clone()),
            ("mirror omitted", omitted.clone()),
        ] {
            let decoded = decode_wire(payload.clone())
                .unwrap_or_else(|error| panic!("{shape}: the delivery owns the identity: {error}"));
            let decoded = Instrument::new(decoded).expect("the decoded terms build an instrument");
            assert_eq!(decoded, instrument, "{shape}: both shapes are one contract");
            let InstrumentTerms::Option(terms) = decoded.terms() else {
                panic!("fixture must be an option")
            };
            assert_eq!(
                terms.conversion_reference(),
                Some(&cross_id),
                "{shape}: the identity is the delivery's"
            );
        }

        let mut contradicting = mirrored;
        contradicting["spec"]["conversion_reference"] = serde_json::json!(
            conversion(venue_token("ETH"), &usd())
                .reference_id()
                .expect("reference should be valid")
                .as_str()
        );
        assert_eq!(
            decode_wire(contradicting.clone()),
            Err(ModelError::ConversionReferenceMismatch)
        );
        assert_public_decode_refuses(
            "cross-asset option with a contradicting mirror",
            contradicting,
        );

        let mut foreign = omitted;
        foreign["spec"]["conversion_reference"] = serde_json::json!(cross_id.as_str());
        foreign["spec"]["delivery"]["mode"]["conversion_reference"] = serde_json::Value::Null;
        assert_eq!(
            decode_wire(foreign.clone()),
            Err(ModelError::RequiredConversionReference)
        );
        assert_public_decode_refuses(
            "cross-asset option mirrored without a delivery conversion",
            foreign,
        );
    }

    #[test]
    fn wire_accepted_terms_should_be_accepted_by_their_constructors() {
        let cross_conversion = conversion(venue_token("BTC"), &usd());
        let quote_conversion = conversion(usd(), &usd());
        let cross_asset_delivery = || {
            DeliveryMode::cash(
                venue_token("BTC"),
                cash_reference(),
                Some(&cross_conversion),
            )
            .expect("the cross-asset delivery is buildable")
        };
        let quote_settled_carrying_conversion = || {
            DeliveryMode::cash(usd(), cash_reference(), Some(&quote_conversion))
                .expect("a delivery that declares a conversion is buildable")
        };

        let constructed = [
            Instrument::new(InstrumentTerms::Perpetual(
                PerpetualTerms::new(
                    underlying("BTC"),
                    usd(),
                    venue_token("BTC"),
                    quote_contract_size(100),
                    DerivativePnlModel::Inverse,
                    Some(&cross_conversion),
                )
                .expect("terms should be valid"),
            ))
            .expect("instrument should be valid"),
            Instrument::new(InstrumentTerms::Future(
                FutureTerms::new(
                    underlying("BTC"),
                    usd(),
                    DerivativePnlModel::Quanto,
                    contract_size("BTC", 5),
                    expiry(),
                    None,
                    cross_asset_delivery(),
                    Some(&cross_conversion),
                )
                .expect("terms should be valid"),
            ))
            .expect("instrument should be valid"),
            Instrument::new(InstrumentTerms::Option(cross_asset_cash_option(
                OptionRight::Call,
                100,
                1,
            )))
            .expect("instrument should be valid"),
        ];
        for instrument in constructed {
            let encoded = serde_json::to_value(&instrument).expect("instrument should serialize");
            let decoded = decode_wire(encoded).expect("the wire accepts the constructor shape");
            assert_eq!(
                Instrument::new(decoded).expect("instrument").id(),
                instrument.id()
            );
        }

        assert_same_decision(
            "linear perpetual with a conversion",
            decode_wire({
                let mut payload = serde_json::to_value(
                    Instrument::new(InstrumentTerms::Perpetual(
                        PerpetualTerms::new(
                            underlying("BTC"),
                            usd(),
                            usd(),
                            contract_size("BTC", 1),
                            DerivativePnlModel::Linear,
                            None,
                        )
                        .expect("terms should be valid"),
                    ))
                    .expect("instrument should be valid"),
                )
                .expect("serialize");
                payload["spec"]["conversion_reference"] =
                    serde_json::json!(cross_conversion.reference_id().expect("id").as_str());
                payload
            }),
            PerpetualTerms::with_conversion_reference(
                underlying("BTC"),
                usd(),
                usd(),
                contract_size("BTC", 1),
                DerivativePnlModel::Linear,
                Some(cross_conversion.reference_id().expect("id")),
            )
            .map(InstrumentTerms::Perpetual),
        );
        assert_same_decision(
            "physical future handed a conversion reference",
            decode_wire({
                let physical = || {
                    DeliveryMode::physical(vec![
                        DeliveryLeg::new(
                            venue_token("BTC"),
                            Quantity::from(5_u8),
                            DeliveryDirection::Deliver,
                        )
                        .expect("leg should be valid"),
                    ])
                    .expect("delivery should be valid")
                };
                let mut payload = serde_json::to_value(
                    Instrument::new(InstrumentTerms::Future(
                        FutureTerms::new(
                            underlying("BTC"),
                            usd(),
                            DerivativePnlModel::Linear,
                            contract_size("BTC", 5),
                            expiry(),
                            None,
                            physical(),
                            None,
                        )
                        .expect("terms should be valid"),
                    ))
                    .expect("instrument should be valid"),
                )
                .expect("serialize");
                payload["spec"]["conversion_reference"] =
                    serde_json::json!(cross_conversion.reference_id().expect("id").as_str());
                payload
            }),
            FutureTerms::new(
                underlying("BTC"),
                usd(),
                DerivativePnlModel::Linear,
                contract_size("BTC", 5),
                expiry(),
                None,
                DeliveryMode::physical(vec![
                    DeliveryLeg::new(
                        venue_token("BTC"),
                        Quantity::from(5_u8),
                        DeliveryDirection::Deliver,
                    )
                    .expect("leg should be valid"),
                ])
                .expect("delivery should be valid"),
                Some(&cross_conversion),
            )
            .map(InstrumentTerms::Future),
        );

        assert_same_decision(
            "quote-settled cash option with a delivery conversion",
            decode_wire({
                let mut payload = serde_json::to_value(
                    Instrument::new(InstrumentTerms::Option(cash_option(
                        OptionRight::Call,
                        100,
                        1,
                    )))
                    .expect("instrument should be valid"),
                )
                .expect("serialize");
                payload["spec"]["delivery"]["mode"]["conversion_reference"] =
                    serde_json::json!(quote_conversion.reference_id().expect("id").as_str());
                payload
            }),
            OptionTerms::new(
                underlying("BTC"),
                usd(),
                OptionRight::Call,
                strike(100),
                expiry(),
                ExerciseStyle::European,
                contract_size("BTC", 1),
                quote_settled_carrying_conversion(),
                None,
            )
            .map(InstrumentTerms::Option),
        );
    }

    #[test]
    fn inverse_perpetual_should_settle_in_its_own_quote_without_a_conversion() {
        let degenerate = || crate::asset::DerivativeUnderlying {
            reference: UnderlyingRef::Asset(usd()),
            price_reference: reference(
                UnderlyingRef::Asset(usd()),
                usd(),
                PriceBasis::PerAssetUnit,
            ),
        };

        let terms = PerpetualTerms::new(
            degenerate(),
            usd(),
            usd(),
            quote_contract_size(100),
            DerivativePnlModel::Inverse,
            None,
        )
        .expect("a settlement in the price quote needs no conversion");
        assert_eq!(terms.settlement_asset(), &usd());
        assert_eq!(terms.conversion_reference(), None);

        assert_eq!(
            PerpetualTerms::new(
                degenerate(),
                usd(),
                usd(),
                quote_contract_size(100),
                DerivativePnlModel::Inverse,
                Some(&conversion(usd(), &usd())),
            ),
            Err(ModelError::UnexpectedConversionReference),
            "the contract converts nothing, so it must not declare a reference"
        );
    }

    #[test]
    fn wire_should_omit_an_absent_conversion_reference_in_every_family() {
        let conversion = conversion(venue_token("BTC"), &usd());
        let absent = [
            (
                "perpetual",
                Instrument::new(InstrumentTerms::Perpetual(
                    PerpetualTerms::new(
                        underlying("BTC"),
                        usd(),
                        usd(),
                        contract_size("BTC", 1),
                        DerivativePnlModel::Linear,
                        None,
                    )
                    .expect("terms should be valid"),
                ))
                .expect("instrument should be valid"),
            ),
            (
                "future",
                Instrument::new(InstrumentTerms::Future(
                    FutureTerms::new(
                        underlying("BTC"),
                        usd(),
                        DerivativePnlModel::Linear,
                        contract_size("BTC", 5),
                        expiry(),
                        None,
                        cash_delivery(),
                        None,
                    )
                    .expect("terms should be valid"),
                ))
                .expect("instrument should be valid"),
            ),
            (
                "option",
                Instrument::new(InstrumentTerms::Option(cash_option(
                    OptionRight::Call,
                    100,
                    1,
                )))
                .expect("instrument should be valid"),
            ),
        ];
        for (family, instrument) in absent {
            let encoded =
                serde_json::to_value(&instrument).expect("the instrument should serialize");
            assert!(
                encoded["spec"].get("conversion_reference").is_none(),
                "a {family} without a conversion reference must not serialize the member"
            );
            if let Some(mode) = encoded["spec"]
                .get("delivery")
                .and_then(|delivery| delivery.get("mode"))
            {
                assert!(
                    mode.get("conversion_reference").is_none(),
                    "a {family} delivery without a conversion reference must not serialize the member"
                );
            }
        }

        let cross_asset_delivery =
            DeliveryMode::cash(venue_token("BTC"), cash_reference(), Some(&conversion))
                .expect("the cross-asset delivery is buildable");
        let present = [
            (
                "perpetual",
                Instrument::new(InstrumentTerms::Perpetual(
                    PerpetualTerms::new(
                        underlying("BTC"),
                        usd(),
                        venue_token("BTC"),
                        quote_contract_size(100),
                        DerivativePnlModel::Inverse,
                        Some(&conversion),
                    )
                    .expect("terms should be valid"),
                ))
                .expect("instrument should be valid"),
            ),
            (
                "future",
                Instrument::new(InstrumentTerms::Future(
                    FutureTerms::new(
                        underlying("BTC"),
                        usd(),
                        DerivativePnlModel::Quanto,
                        contract_size("BTC", 5),
                        expiry(),
                        None,
                        cross_asset_delivery,
                        Some(&conversion),
                    )
                    .expect("terms should be valid"),
                ))
                .expect("instrument should be valid"),
            ),
            (
                "option",
                Instrument::new(InstrumentTerms::Option(cross_asset_cash_option(
                    OptionRight::Call,
                    100,
                    1,
                )))
                .expect("instrument should be valid"),
            ),
        ];
        for (family, instrument) in present {
            let encoded =
                serde_json::to_value(&instrument).expect("the instrument should serialize");
            assert!(
                encoded["spec"].get("conversion_reference").is_some(),
                "a cross-asset {family} must state the conversion identity"
            );
            let decoded =
                serde_json::from_value::<Instrument>(encoded).expect("the payload should decode");
            assert_eq!(
                decoded.id(),
                instrument.id(),
                "the {family} must round-trip"
            );
            assert_eq!(decoded, instrument);
        }
    }

    #[test]
    fn quote_settled_cash_future_should_encode_the_previous_bytes() {
        let terms = FutureTerms::new(
            underlying("BTC"),
            usd(),
            DerivativePnlModel::Linear,
            contract_size("BTC", 5),
            expiry(),
            Some(1_700),
            cash_delivery(),
            None,
        )
        .expect("terms should be valid");
        assert_eq!(terms.delivery().cash_conversion_reference(), None);

        let mut encoder = crate::identifier::CanonicalEncoder::new();
        terms.encode(&mut encoder);
        let mut expected = terms_bytes(&underlying("BTC"));
        expected.extend_from_slice(&terms_bytes(&usd()));
        expected.extend_from_slice(&terms_bytes(&DerivativePnlModel::Linear));
        expected.extend_from_slice(&terms_bytes(&contract_size("BTC", 5)));
        expected.extend_from_slice(&terms_bytes(&expiry()));
        expected.push(1);
        expected.extend_from_slice(&1_700_i64.to_be_bytes());
        expected.extend_from_slice(&previous_cash_delivery_bytes(&usd(), &cash_reference()));

        assert_eq!(encoder.as_bytes(), expected.as_slice());
    }

    #[test]
    fn physical_future_should_encode_the_previous_bytes() {
        let mut deliveries = vec![
            DeliveryLeg::new(
                venue_token("BTC"),
                Quantity::from(5_u8),
                DeliveryDirection::Deliver,
            )
            .expect("leg should be valid"),
        ];
        deliveries.sort();
        let terms = FutureTerms::new(
            underlying("BTC"),
            usd(),
            DerivativePnlModel::Linear,
            contract_size("BTC", 5),
            expiry(),
            None,
            DeliveryMode::physical(deliveries.clone()).expect("delivery should be valid"),
            None,
        )
        .expect("terms should be valid");

        let mut encoder = crate::identifier::CanonicalEncoder::new();
        terms.encode(&mut encoder);
        let mut expected = terms_bytes(&underlying("BTC"));
        expected.extend_from_slice(&terms_bytes(&usd()));
        expected.extend_from_slice(&terms_bytes(&DerivativePnlModel::Linear));
        expected.extend_from_slice(&terms_bytes(&contract_size("BTC", 5)));
        expected.extend_from_slice(&terms_bytes(&expiry()));
        expected.push(0);
        expected.extend_from_slice(&physical_delivery_bytes(&deliveries));

        assert_eq!(encoder.as_bytes(), expected.as_slice());
    }

    #[test]
    fn quote_settled_cash_option_should_encode_the_previous_bytes() {
        let terms = cash_option(OptionRight::Call, 100, 2);
        assert_eq!(terms.conversion_reference(), None);

        let mut encoder = crate::identifier::CanonicalEncoder::new();
        terms.encode(&mut encoder);
        let mut expected = terms_bytes(&underlying("BTC"));
        expected.extend_from_slice(&terms_bytes(&usd()));
        expected.extend_from_slice(&terms_bytes(&OptionRight::Call));
        expected.extend_from_slice(&terms_bytes(&strike(100)));
        expected.extend_from_slice(&terms_bytes(&expiry()));
        expected.extend_from_slice(&terms_bytes(&ExerciseStyle::European));
        expected.extend_from_slice(&terms_bytes(&contract_size("BTC", 2)));
        expected.extend_from_slice(&previous_cash_delivery_bytes(&usd(), &cash_reference()));

        assert_eq!(encoder.as_bytes(), expected.as_slice());
    }

    #[test]
    fn cross_asset_cash_future_should_encode_the_previous_bytes_and_its_conversion() {
        let conversion = conversion(venue_token("BTC"), &usd());
        let conversion_id = conversion
            .reference_id()
            .expect("reference should be valid");
        let delivery = DeliveryMode::cash(venue_token("BTC"), cash_reference(), Some(&conversion))
            .expect("the cross-asset delivery is buildable");
        let terms = FutureTerms::new(
            underlying("BTC"),
            usd(),
            DerivativePnlModel::Quanto,
            contract_size("BTC", 5),
            expiry(),
            Some(1_700),
            delivery,
            Some(&conversion),
        )
        .expect("a quanto future is a contract this model has");
        assert_eq!(
            terms.delivery().cash_conversion_reference(),
            Some(&conversion_id)
        );

        let mut encoder = crate::identifier::CanonicalEncoder::new();
        terms.encode(&mut encoder);
        let mut expected = terms_bytes(&underlying("BTC"));
        expected.extend_from_slice(&terms_bytes(&usd()));
        expected.extend_from_slice(&terms_bytes(&DerivativePnlModel::Quanto));
        expected.extend_from_slice(&terms_bytes(&contract_size("BTC", 5)));
        expected.extend_from_slice(&terms_bytes(&expiry()));
        expected.push(1);
        expected.extend_from_slice(&1_700_i64.to_be_bytes());
        expected.extend_from_slice(&cash_delivery_with_conversion_bytes(
            &venue_token("BTC"),
            &cash_reference(),
            &conversion_id,
        ));

        assert_eq!(
            encoder.as_bytes(),
            expected.as_slice(),
            "the conversion reference is exactly a marker and the identity it names"
        );
    }

    #[test]
    fn cross_asset_cash_option_should_encode_the_previous_bytes_and_its_conversion() {
        let conversion = conversion(venue_token("BTC"), &usd());
        let conversion_id = conversion
            .reference_id()
            .expect("reference should be valid");
        let delivery = DeliveryMode::cash(venue_token("BTC"), cash_reference(), Some(&conversion))
            .expect("the cross-asset delivery is buildable");
        let terms = OptionTerms::new(
            underlying("BTC"),
            usd(),
            OptionRight::Call,
            strike(100),
            expiry(),
            ExerciseStyle::European,
            contract_size("BTC", 2),
            delivery,
            Some(&conversion),
        )
        .expect("a cross-asset cash option is a contract this model has");
        assert_eq!(terms.conversion_reference(), Some(&conversion_id));

        let mut encoder = crate::identifier::CanonicalEncoder::new();
        terms.encode(&mut encoder);
        let mut expected = terms_bytes(&underlying("BTC"));
        expected.extend_from_slice(&terms_bytes(&usd()));
        expected.extend_from_slice(&terms_bytes(&OptionRight::Call));
        expected.extend_from_slice(&terms_bytes(&strike(100)));
        expected.extend_from_slice(&terms_bytes(&expiry()));
        expected.extend_from_slice(&terms_bytes(&ExerciseStyle::European));
        expected.extend_from_slice(&terms_bytes(&contract_size("BTC", 2)));
        expected.extend_from_slice(&cash_delivery_with_conversion_bytes(
            &venue_token("BTC"),
            &cash_reference(),
            &conversion_id,
        ));

        assert_eq!(
            encoder.as_bytes(),
            expected.as_slice(),
            "the conversion reference is exactly a marker and the identity it names"
        );
    }

    #[test]
    fn physical_option_should_encode_the_previous_bytes() {
        let mut deliveries = vec![
            DeliveryLeg::new(
                venue_token("BTC"),
                Quantity::from(2_u8),
                DeliveryDirection::Receive,
            )
            .expect("leg should be valid"),
            DeliveryLeg::new(usd(), Quantity::from(200_u8), DeliveryDirection::Deliver)
                .expect("leg should be valid"),
        ];
        deliveries.sort();
        let terms = OptionTerms::new(
            underlying("BTC"),
            usd(),
            OptionRight::Call,
            strike(100),
            expiry(),
            ExerciseStyle::European,
            contract_size("BTC", 2),
            DeliveryMode::physical(deliveries.clone()).expect("delivery should be valid"),
            None,
        )
        .expect("terms should be valid");
        assert_eq!(terms.conversion_reference(), None);

        let mut encoder = crate::identifier::CanonicalEncoder::new();
        terms.encode(&mut encoder);
        let mut expected = terms_bytes(&underlying("BTC"));
        expected.extend_from_slice(&terms_bytes(&usd()));
        expected.extend_from_slice(&terms_bytes(&OptionRight::Call));
        expected.extend_from_slice(&terms_bytes(&strike(100)));
        expected.extend_from_slice(&terms_bytes(&expiry()));
        expected.extend_from_slice(&terms_bytes(&ExerciseStyle::European));
        expected.extend_from_slice(&terms_bytes(&contract_size("BTC", 2)));
        expected.extend_from_slice(&physical_delivery_bytes(&deliveries));

        assert_eq!(encoder.as_bytes(), expected.as_slice());
    }
}
