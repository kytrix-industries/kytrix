//! The five product families and their canonical instrument identity.
//!
//! An [`Instrument`] describes *what* is traded and *how it settles*; it never
//! describes where or under which native identifier it trades. Every field is an
//! economic term: changing any of them changes the [`InstrumentId`], and no
//! ticker, native symbol, route, listing rule or status can influence it.
//! The term fields are private to this module: the validated constructors of each
//! family are the only way to build terms, and a registry rehydrates them by
//! decoding the wire shape through those same constructors rather than by
//! writing fields.

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

/// The canonical terms of a spot instrument: two distinct assets exchanged
/// immediately against each other.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct SpotTerms {
    #[serde(rename = "base")]
    base: AssetId,
    #[serde(rename = "quote")]
    quote: AssetId,
}

impl SpotTerms {
    /// Validates the spot terms.
    pub fn new(base: AssetId, quote: AssetId) -> Result<Self, ModelError> {
        if base == quote {
            return Err(ModelError::IdenticalSpotAssets);
        }
        Ok(Self { base, quote })
    }

    /// Returns the base asset.
    pub fn base(&self) -> &AssetId {
        &self.base
    }

    /// Returns the quote asset.
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

/// The canonical terms of a perpetual instrument.
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
    /// The registry identity of the conversion reference, when the settlement
    /// asset differs from the price quote.
    ///
    /// The identity fixes which reference the contract converts through and
    /// nothing else: its direction is a property of the registry terms behind the
    /// digest, and an identity is a one-way digest, so the direction is not
    /// recoverable from it. This type stores the identity and does not check —
    /// and does not pretend to check — the direction it names.
    ///
    /// # Obligations
    ///
    /// The market participant that publishes the contract **must** resolve the identity in
    /// `ReferenceRegistry`, refuse the publication with its own missing-reference
    /// error when no terms are registered for it, and reject the instrument when
    /// those terms do not convert the settlement asset into the price quote per
    /// asset unit.
    #[serde(
        rename = "conversion_reference",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    conversion_reference: Option<PriceReferenceId>,
}

impl PerpetualTerms {
    /// Validates the perpetual terms.
    ///
    /// The terms fix the whole conversion direction: the pnl model decides
    /// whether the settlement asset may differ from the price quote, and a
    /// differing settlement asset requires `conversion` to be the registry terms
    /// of the conversion reference that converts the settlement asset into the
    /// price quote. The reference is validated and reduced to its identity here,
    /// so no caller can install an unvalidated direction.
    pub fn new(
        underlying: DerivativeUnderlying,
        price_quote: AssetId,
        settlement_asset: AssetId,
        contract_size: ContractSize,
        pnl_model: DerivativePnlModel,
        conversion: Option<&PriceReferenceTerms>,
    ) -> Result<Self, ModelError> {
        // A perpetual has no delivery mode, so its terms are the only statement of a
        // conversion: the identity the contract stores is the one they name, and a caller
        // is the only entry that holds the terms behind it.
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

    /// Validates the perpetual terms of an instrument decoded from the wire.
    ///
    /// A decoded payload carries the derived conversion identity rather than the
    /// registry terms it came from, and an identity is a one-way digest: the
    /// direction of the reference — its underlying, its quote and its basis — is
    /// not recoverable from it and is **not** checked here. The identity says which
    /// reference the contract converts through, and every other rule is the one
    /// [`PerpetualTerms::new`] applies, because both entries run [`Self::build`].
    ///
    /// # Obligations
    ///
    /// The publishing market participant **must** resolve `conversion_reference` in
    /// `ReferenceRegistry` and check its direction; nothing on this path does.
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

    /// Re-checks the invariants of already constructed terms.
    ///
    /// The terms can only have been built by [`Self::build`], so this runs that same
    /// entry again over the stored terms rather than installing a second set of rules:
    /// the judgement is the one the constructor made, and the evidence it re-states is
    /// exactly the identity the terms store.
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

    /// The one entry that builds and validates these terms.
    ///
    /// Every other constructor, the wire decoder and [`Self::validate`] call this
    /// function, so no rule of this family is stated twice and no entry can accept a
    /// contract another entry refuses.
    fn build(
        underlying: DerivativeUnderlying,
        price_quote: AssetId,
        settlement_asset: AssetId,
        contract_size: ContractSize,
        pnl_model: DerivativePnlModel,
        conversion: ConversionSource<'_>,
    ) -> Result<Self, ModelError> {
        // A perpetual has no delivery mode, so its terms are the only statement of a
        // conversion: which settlement assets the pnl model can reach is judged
        // first, so a settlement the contract cannot reach is refused for that
        // settlement rather than for the reference the caller did or did not give.
        // The two families with a delivery mode judge a cash settlement the same way,
        // so a payload and a caller that describe one contract are refused for one
        // reason.
        validate_derivative_terms(
            &underlying,
            &price_quote,
            &settlement_asset,
            &contract_size,
            pnl_model,
            None,
        )?;
        // A perpetual has no delivery mode, so the pnl model is the half of the
        // convertibility judgement a delivery would otherwise state: a linear perpetual
        // pays the quote price of its underlying and settles in that quote, while the
        // other two models settle elsewhere.
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

    /// Returns the derivative underlying.
    pub fn underlying(&self) -> &DerivativeUnderlying {
        &self.underlying
    }

    /// Returns the price quote asset.
    pub fn price_quote(&self) -> &AssetId {
        &self.price_quote
    }

    /// Returns the settlement asset.
    pub fn settlement_asset(&self) -> &AssetId {
        &self.settlement_asset
    }

    /// Returns the contract size.
    pub fn contract_size(&self) -> &ContractSize {
        &self.contract_size
    }

    /// Returns the pnl model.
    pub fn pnl_model(&self) -> DerivativePnlModel {
        self.pnl_model
    }

    /// Returns the conversion reference that converts the settlement asset into
    /// the price quote, when the two differ.
    ///
    /// The reference is a registry identity, so its direction can only be checked
    /// against the registry terms it was derived from: a decoded identity is
    /// whatever its producer installed, and no reader of this value can tell a
    /// correct direction from a forged one.
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
        // Quote-settled perpetuals carry no conversion reference, so the marker
        // is written only for the cross-asset case; a contract without one keeps
        // the bytes of the pre-conversion-reference version of this shape.
        if let Some(conversion) = &self.conversion_reference {
            encoder.write_label("conversion-reference");
            encoder.write_terms(conversion);
        }
    }
}

/// The canonical terms of a future instrument.
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
    /// Validates the future terms.
    ///
    /// A future always has an expiry, its first notice instant never falls after
    /// the last trading instant, and its delivery mode has to agree with the
    /// contract size and pnl model.
    ///
    /// The cash delivery is the only place these terms store a conversion
    /// reference, so it decides what the contract converts through: a cash
    /// settlement in the price quote and a physical delivery must not declare one,
    /// and a cross-asset cash delivery must be given exactly the terms of the
    /// reference it declares, so the direction can never be dropped or replaced at
    /// this boundary.
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
        // The delivery owns the identity of the conversion this family has, so it is the
        // statement of the contract; the terms a caller supplies have to be the terms of
        // exactly that reference.
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

    /// Validates the future terms of an instrument decoded from the wire.
    ///
    /// The cash delivery is the only place these terms store a conversion
    /// reference, so it decides what the decoded contract converts through: the
    /// instrument-level wire id mirrors that identity and can neither replace it
    /// nor introduce one the delivery does not carry. A payload whose two
    /// statements disagree is refused rather than resolved in favour of either.
    ///
    /// A decoded payload carries the derived identity rather than the registry
    /// terms it came from, and an identity is a one-way digest: the direction of
    /// the reference is not recoverable from it and is **not** checked here. Every
    /// other rule is the one [`FutureTerms::new`] applies, because both entries run
    /// [`Self::build`].
    ///
    /// # Obligations
    ///
    /// The publishing market participant **must** resolve the identity the delivery carries
    /// in `ReferenceRegistry`, refuse the publication when no terms are registered
    /// for it, and check the direction those terms express; nothing on this path
    /// does.
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

    /// Re-checks the invariants of already constructed terms.
    ///
    /// The terms can only have been built by [`Self::build`], so this runs that same
    /// entry again over the stored terms rather than installing a second set of rules:
    /// the judgement is the one the constructor made, and the evidence it re-states is
    /// exactly the identity the stored delivery carries.
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

    /// The one entry that builds and validates these terms.
    ///
    /// Every other constructor, the wire decoder and [`Self::validate`] call this
    /// function, so no rule of this family is stated twice and no entry can accept a
    /// contract another entry refuses.
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
        // The delivery is the settlement the contract states, so it is judged as a
        // settlement before anything is derived from it: the same payload on the wire
        // is refused while its delivery is decoded, which is before its instrument
        // terms are read at all.
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
        // The delivery owns the identity in this family, so resolving the statement is the
        // check and there is nothing left to store: the terms keep the delivery the
        // resolved identity came from, and no path here can store another one.
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

    /// Returns the derivative underlying.
    pub fn underlying(&self) -> &DerivativeUnderlying {
        &self.underlying
    }

    /// Returns the price quote asset.
    pub fn price_quote(&self) -> &AssetId {
        &self.price_quote
    }

    /// Returns the pnl model.
    pub fn pnl_model(&self) -> DerivativePnlModel {
        self.pnl_model
    }

    /// Returns the contract size.
    pub fn contract_size(&self) -> &ContractSize {
        &self.contract_size
    }

    /// Returns the contract expiry.
    pub fn expiry(&self) -> &Expiry {
        &self.expiry
    }

    /// Returns the first notice instant.
    pub fn first_notice_at(&self) -> Option<UnixMillis> {
        self.first_notice_at
    }

    /// Returns the delivery mode.
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

/// The canonical terms of an option instrument.
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
    /// Validates the option terms.
    ///
    /// The strike quote asset and basis must match the option, the exercise
    /// schedule must fall inside the expiry, and the delivery mode must agree
    /// with the contract size and right.
    ///
    /// An option has no pnl model, so the generic `Linear => S == Q` rule does not
    /// apply to it: a cash option settles in its price quote without a conversion
    /// reference and in any other settlement asset only with one, and a physical
    /// option has no conversion reference at all. `conversion` carries the
    /// registry terms of that reference when the settlement asset differs from the
    /// price quote.
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
        // The delivery owns the identity of the conversion this family has, so it is the
        // statement of the contract; the terms a caller supplies have to be the terms of
        // exactly that reference.
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

    /// Validates the option terms of an instrument decoded from the wire.
    ///
    /// The presence rules of [`OptionTerms::new`] are applied here unchanged, because
    /// they only depend on the terms a decoded payload already carries: a cash option
    /// that settles in its price quote must not declare a conversion, a cash option
    /// that settles elsewhere must declare one, and a physical option has no cash
    /// settlement to convert. The instrument-level wire id is only a mirror of the
    /// delivery's identity and is refused when it disagrees with it.
    ///
    /// A decoded payload carries the derived identity rather than the registry
    /// terms it came from, and an identity is a one-way digest: the direction of
    /// the reference is not recoverable from it and is **not** checked here. Every
    /// other rule is the one [`OptionTerms::new`] applies, because both entries run
    /// [`Self::build`].
    ///
    /// # Obligations
    ///
    /// The publishing market participant **must** resolve the identity this delivery carries
    /// in `ReferenceRegistry`, refuse the publication when no terms are registered
    /// for it, and check the direction those terms express; nothing on this path
    /// does.
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

    /// Re-checks the invariants of already constructed terms.
    ///
    /// The terms can only have been built by [`Self::build`], so this runs that same
    /// entry again over the stored terms rather than installing a second set of rules:
    /// the judgement is the one the constructor made, and the evidence it re-states is
    /// exactly the identity the stored delivery carries.
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

    /// The one entry that builds and validates these terms.
    ///
    /// Every other constructor, the wire decoder and [`Self::validate`] call this
    /// function, so no rule of this family is stated twice and no entry can accept a
    /// contract another entry refuses.
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
        // The delivery is the settlement the contract states, so it is judged as a
        // settlement before anything is derived from it, which is the point the wire
        // refuses the same payload at.
        delivery.validate()?;
        expiry.validate()?;
        exercise.validate(&expiry)?;
        if strike.quote_asset() != &price_quote {
            return Err(ModelError::StrikeQuoteMismatch);
        }
        if strike.basis() != &expected_price_basis(&underlying, &contract_size) {
            return Err(ModelError::IncompatiblePriceBasis);
        }
        // An option has no pnl model, so the generic `Linear => S == Q` rule cannot
        // decide its conversion; the delivery states whether the cash settlement
        // leaves the price quote, and the terms a caller supplies have to be the
        // registry terms of exactly the reference the delivery declares.
        let settlement_asset = delivery.cash_asset().unwrap_or(&price_quote).clone();
        // The delivery owns the identity in this family, so resolving the statement is the
        // check and there is nothing left to store: the terms keep the delivery the
        // resolved identity came from, and no path here can store another one.
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

    /// Returns the derivative underlying.
    pub fn underlying(&self) -> &DerivativeUnderlying {
        &self.underlying
    }

    /// Returns the price quote asset.
    pub fn price_quote(&self) -> &AssetId {
        &self.price_quote
    }

    /// Returns the option right.
    pub fn right(&self) -> OptionRight {
        self.right
    }

    /// Returns the strike.
    pub fn strike(&self) -> &StrikePrice {
        &self.strike
    }

    /// Returns the contract expiry.
    pub fn expiry(&self) -> &Expiry {
        &self.expiry
    }

    /// Returns the exercise style.
    pub fn exercise(&self) -> &ExerciseStyle {
        &self.exercise
    }

    /// Returns the contract size.
    pub fn contract_size(&self) -> &ContractSize {
        &self.contract_size
    }

    /// Returns the delivery mode.
    pub fn delivery(&self) -> &DeliveryMode {
        &self.delivery
    }

    /// Returns the conversion reference that converts the cash settlement asset
    /// into the price quote, when the two differ.
    ///
    /// A physical option has no cash settlement at all and therefore no
    /// conversion reference. The returned value is a registry identity: its
    /// direction is only knowable from the registry terms it was derived from, so
    /// a decoded identity is verified there by whoever publishes it.
    pub fn conversion_reference(&self) -> Option<&PriceReferenceId> {
        self.delivery.cash_conversion_reference()
    }

    /// Calculates the typed cash payoff of one cash-settled option contract.
    ///
    /// `reference` is the settlement reference price observed at expiry, in the
    /// price quote asset of these terms. A call pays
    /// `max(reference - strike, 0) * contract_size` and a put pays
    /// `max(strike - reference, 0) * contract_size`; a long position that expires
    /// worthless pays zero.
    ///
    /// `settlement` states how the payoff reaches the cash settlement asset. A
    /// quote-settled option passes [`OptionSettlementObservation::Quote`] and the
    /// result is denominated in the price quote. A cross-asset option cannot be
    /// valued from a per-price quote alone, so the caller must pass the amount it
    /// already converted through the conversion reference of these terms; the
    /// observation is checked against the declared reference and settlement asset
    /// instead of being trusted, and a missing or foreign conversion is rejected
    /// rather than being reported as quote units of another asset.
    ///
    /// A physically settled option has no cash payoff, and neither an index-point
    /// nor an inverse contract can be valued from a single per-asset price, so both
    /// are rejected instead of returning a guessed number.
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
        // A cash payoff is a quantity of the asset the contract tracks times a price of
        // that asset, so a contract that tracks no asset at all cannot be valued from one
        // per-asset price.
        let Some(underlying_asset) = self.underlying.reference().asset() else {
            return Err(ModelError::UnderlyingWithoutAsset);
        };
        // An inverse contract states its contract size as a notional of the price
        // quote while it delivers the underlying, so the notional is a divisor
        // rather than a multiplier and a quote price alone cannot value it.
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

/// The canonical terms of an outcome instrument.
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
    /// Builds outcome terms from an already resolved group/member/side.
    pub fn new(group: OutcomeGroupId, member: OutcomeMemberId, side: OutcomeSideId) -> Self {
        Self {
            group,
            member,
            side,
        }
    }

    /// Returns the outcome group identity.
    pub fn group(&self) -> &OutcomeGroupId {
        &self.group
    }

    /// Returns the outcome member identity.
    pub fn member(&self) -> &OutcomeMemberId {
        &self.member
    }

    /// Returns the outcome side identity.
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

/// The wire shape of an outcome instrument: the three resolved identities its
/// contract is made of and nothing else.
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

    /// Decodes the shape into the terms of this family.
    ///
    /// This family states no rule that spans its three identities: group, member and side
    /// are each decoded by the `Deserialize` of their own identity type, which refuses an
    /// identity that was never published, and assembling three resolved identities adds
    /// nothing to check. The conversion is therefore always successful, and the decoding is
    /// the only place a malformed statement is refused.
    fn try_from(wire: OutcomeTermsWire) -> Result<Self, Self::Error> {
        Ok(Self::new(wire.group, wire.member, wire.side))
    }
}

/// The members of the outcome wire shape, in the order a sequence states them.
const OUTCOME_TERMS_FIELDS: &[&str] = &["group", "member", "side"];

/// Reads the outcome wire shape and hands it to the constructor of this family.
///
/// The shape itself is read by [`OutcomeTermsWire`]. This visitor exists for the one thing
/// that shape cannot state: its own name, which serde takes from the type it is written
/// on. A decoding error of a public type is read by the caller, so it has to name the type
/// that caller asked for rather than the shape the decoder runs through.
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
        // A sequence states the same three members positionally, so a sequence too short
        // to state them all is refused the way a map missing one is.
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

/// The product family of an instrument.
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
    /// Returns the fixed canonical label of this product family.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Spot => "spot",
            Self::Perpetual => "perpetual",
            Self::Future => "future",
            Self::Option => "option",
            Self::Outcome => "outcome",
        }
    }

    /// Returns `true` when this family is a derivative.
    pub const fn is_derivative(self) -> bool {
        matches!(self, Self::Perpetual | Self::Future | Self::Option)
    }
}

impl fmt::Display for InstrumentKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// The validated canonical terms of an instrument.
///
/// Terms can only be built through the validated constructors of each family, so
/// an [`InstrumentId`] can never be derived from unchecked field values.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InstrumentTerms {
    Spot(SpotTerms),
    Perpetual(PerpetualTerms),
    Future(FutureTerms),
    Option(OptionTerms),
    Outcome(OutcomeTerms),
}

impl InstrumentTerms {
    /// Returns the product family of these terms.
    pub const fn kind(&self) -> InstrumentKind {
        match self {
            Self::Spot(_) => InstrumentKind::Spot,
            Self::Perpetual(_) => InstrumentKind::Perpetual,
            Self::Future(_) => InstrumentKind::Future,
            Self::Option(_) => InstrumentKind::Option,
            Self::Outcome(_) => InstrumentKind::Outcome,
        }
    }

    /// Validates the terms against their product family invariants.
    ///
    /// Every family re-runs the rules of its own validated constructor over the
    /// stored terms rather than installing a second set of them: the terms can only
    /// have been built through those constructors, so this is the same judgement
    /// applied again and never a different one.
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

    /// Returns the price basis implied by these terms, when they are dated.
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

/// An immutable economic instrument: its canonical terms and derived identity.
///
/// The type implements `Clone`, `Debug`, `PartialEq`, `Eq`, `Hash` and
/// serialization and deliberately does not implement `Ord`/`PartialOrd`: the
/// catalog orders by the full listing key `(route, listing_id)`, and an instrument
/// selector orders by its own fixed key `(name, canonical route, VenueListingId)` —
/// the instrument identity is never either. The serialized shape is the canonical
/// terms of the instrument; the identity is always recomputed from those terms,
/// never read back from the wire.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Instrument {
    id: InstrumentId,
    terms: InstrumentTerms,
}

impl Instrument {
    /// Validates canonical terms and derives the instrument identity.
    pub fn new(terms: InstrumentTerms) -> Result<Self, ModelError> {
        terms.validate()?;
        let id = InstrumentId::of_terms(&terms);
        Ok(Self { id, terms })
    }

    /// Returns the versioned instrument identity.
    pub fn id(&self) -> &InstrumentId {
        &self.id
    }

    /// Returns the canonical terms.
    ///
    /// The instrument always stores the terms its identity was derived from, so
    /// reading them back can neither fail nor drift from [`Instrument::id`].
    pub fn terms(&self) -> InstrumentTerms {
        self.terms.clone()
    }

    /// Returns the product family of this instrument.
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

/// The wire shape of the five product families.
///
/// The shape is the identity-bearing swap surface, so it mirrors every canonical
/// term field one by one; the derived fields are computed rather than trusted, and
/// the conversion reference is carried explicitly so a decoded cross-asset
/// contract keeps the identity it was encoded with.
///
/// The conversion reference of a future or an option is stored inside its
/// delivery, so the instrument-level `conversion_reference` is a redundant mirror
/// of that identity: it is omitted when the delivery carries none, kept unchanged
/// when it agrees with the delivery, and refused when it contradicts it. An
/// identity is a one-way digest, so decoding one never establishes the reference's
/// direction — that is checked against `ReferenceRegistry` terms by whoever
/// publishes the reference, not by this shape.
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
    /// Mirrors the canonical terms onto the wire shape.
    ///
    /// A perpetual states its conversion identity in its own terms, and a future or an
    /// option states it in the delivery that owns it; both are written where the decoder
    /// reads them from. A future or an option also repeats the same identity at the
    /// instrument level, which is a redundant mirror: it is written whenever the delivery
    /// carries one, and a decoded payload that omits it therefore describes the same
    /// contract.
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

    /// Decodes a payload through the validated constructor of its family.
    ///
    /// Every family is decoded by the one entry that also serves its callers and its
    /// re-validation, so the terms a payload produces are a subset of the terms the
    /// constructors of this module produce by construction rather than by two sets of
    /// rules agreeing. The only statement a payload cannot make is the registry terms
    /// behind a reference identity, so the direction of a conversion reference is left
    /// to whoever publishes that reference; every other rule is decided here.
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
/// The price basis implied by a contract unit and underlying.
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

/// Validates the conversion reference direction of an instrument.
///
/// A conversion reference converts the settlement asset into the price quote, so
/// its registry terms must track `settlement_asset` as the underlying, quote that
/// asset in `price_quote` when the caller knows it, and be expressed per asset
/// unit. Only registry terms are accepted here: the direction is never derived
/// from a route, a native symbol or a reference hash.
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

/// Where a conversion reference's identity comes from.
///
/// A contract states the reference it converts through in one of two ways, and the two
/// differ in one thing only: a caller that holds the registry terms behind the reference
/// can also check the direction those terms express, while an identity is a one-way
/// digest that names only itself. `carrier` is the identity the contract's own statement
/// names — the cash delivery of a future or an option, and the terms themselves for a
/// perpetual, which has no delivery mode to state it — and `mirror` is the redundant
/// instrument-level copy a decoded payload may repeat it with.
///
/// The lower half of the same statement is the cash delivery's own and lives in
/// `settlement`: `CashConversion` there names the same two ways for a delivery, which
/// holds no mirror and derives its identity from the terms themselves. **The two types
/// state one fact at two levels and must stay in step**: a way of stating a conversion
/// that is added to one has to be answered in the other.
pub(crate) enum ConversionSource<'a> {
    /// The caller supplied the registry terms: the identity is derived from them AND the
    /// direction is checked against the settlement asset and the price quote. `terms` is
    /// `None` when the caller declares no conversion at all.
    Registry {
        carrier: Option<PriceReferenceId>,
        terms: Option<&'a PriceReferenceTerms>,
    },
    /// A decoded payload carries only the opaque identity: the identity is trusted as
    /// declared, the direction CANNOT be checked here and is the publishing market participant's
    /// obligation.
    Identity {
        carrier: Option<PriceReferenceId>,
        mirror: Option<PriceReferenceId>,
    },
}

/// Resolves the one conversion identity a contract stores from the statement it was
/// given.
///
/// The same fact is stated in more than one place — a caller holds registry terms, a
/// decoded payload holds an identity inside its settlement and may repeat it at the
/// instrument level — and every entry reduces those statements to one identity here, so
/// no entry can drop a statement, install one the contract does not carry, or decide the
/// presence of a conversion differently from another entry.
///
/// `convertible` states whether the contract performs a conversion at all, and each
/// family derives it from the same `S != Q` judgement: a physical delivery
/// never converts, and a cash settlement converts exactly when it leaves the price quote.
/// It is decided here rather than left to the settlement rules because it is a rule about
/// the reference: a contract that performs no conversion stores no identity, so no
/// statement may introduce one.
///
/// The direction of the reference is checked against the registry terms, which are the
/// only statement that names it. A caller that supplies no terms therefore leaves the
/// direction of the reference it declares unestablished and is refused; a decoded payload
/// holds no terms at all, so the check leaves with whoever published the reference.
///
/// # Obligations
///
/// The publishing market participant **must** resolve the identity in `ReferenceRegistry`, refuse
/// the publication with its own missing-reference error when no terms are registered for
/// it, and reject the instrument when those terms do not convert the settlement asset into
/// the price quote per asset unit.
fn resolve_conversion(
    convertible: bool,
    source: ConversionSource<'_>,
    settlement_asset: &AssetId,
    price_quote: &AssetId,
) -> Result<Option<PriceReferenceId>, ModelError> {
    // The entry's evidence is reduced to the identity the contract stores, the terms that
    // can check its direction, and whether this entry is one that has to hold them.
    let (carrier, terms, mirror, holds_terms) = match source {
        ConversionSource::Registry { carrier, terms } => (carrier, terms, None, true),
        ConversionSource::Identity { carrier, mirror } => (carrier, None, mirror, false),
    };
    if !convertible {
        // A contract that performs no conversion stores no identity, so neither statement
        // may introduce one.
        return if carrier.is_some() || terms.is_some() || mirror.is_some() {
            Err(ModelError::UnexpectedConversionReference)
        } else {
            Ok(None)
        };
    }
    // The mirror never owns the identity: it repeats one the contract already states, so a
    // repetition that contradicts that statement — or that names one the contract does not
    // carry — is a contradiction rather than a replacement.
    if let Some(mirror) = &mirror {
        match &carrier {
            Some(carrier) if carrier != mirror => {
                return Err(ModelError::ConversionReferenceMismatch);
            }
            Some(_) => {}
            None => return Err(ModelError::RequiredConversionReference),
        }
    }
    // A contract that converts has to state which reference it converts through.
    let Some(carrier) = carrier else {
        return Err(ModelError::RequiredConversionReference);
    };
    if holds_terms {
        // Registry terms are the half of the statement that also names the direction, so an
        // entry that holds them is the one that has to establish it.
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

/// Returns `true` when a settlement converts one asset into another.
///
/// A physical delivery converts nothing and a cash settlement converts exactly when
/// its asset is not the price quote, which is the `S != Q` case. The
/// pnl model is deliberately not part of this judgement: it decides which assets a
/// family may settle in, not whether that settlement is a conversion.
pub(crate) fn convertible(
    is_physical: bool,
    settlement_asset: &AssetId,
    price_quote: &AssetId,
) -> bool {
    !is_physical && settlement_asset != price_quote
}

/// Validates the contract unit against the underlying it belongs to.
///
/// An index can only be valued per index point, so an index underlying requires the
/// contract unit to be the quote notional of one index point, carrying the same
/// index and quoting the price quote of the contract. A contract that tracks
/// anything else is a plain quantity of an asset, so an index-point unit on it
/// describes a different contract than the underlying does.
///
/// A cash-settled contract is free to track an index: it is the *settlement asset*
/// that has to be an asset, and that asset is stated by the delivery mode rather
/// than by the underlying.
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

/// Validates that a pnl model can track the underlying it was given.
///
/// An inverse contract is worth a quantity of the asset it tracks divided by that
/// asset's price, so it has to track an asset at all: an index or an external
/// reference has no delivery and no price of its own to divide by. The other models
/// price the quote of what they track and can track an index.
fn validate_pnl_underlying(
    underlying: &DerivativeUnderlying,
    pnl_model: DerivativePnlModel,
) -> Result<(), ModelError> {
    if pnl_model == DerivativePnlModel::Inverse && underlying.reference().asset().is_none() {
        return Err(ModelError::IncompatiblePnlModel);
    }
    Ok(())
}

/// Validates the settlement, contract unit and delivery rules of a perpetual or future.
///
/// The rules are decided in a fixed order, because more than one of them can apply
/// to one payload and only one reason can be reported: what the pnl model may track
/// at all (an inverse contract tracks an asset), what the contract unit therefore has
/// to be, which settlement assets that pnl model can reach, and finally what the
/// physical delivery of a future actually hands over. The conversion identity is not
/// part of this judgement; it follows from these decisions and is resolved by
/// [`resolve_conversion`] over the statement the entry was given.
fn validate_derivative_terms(
    underlying: &DerivativeUnderlying,
    price_quote: &AssetId,
    settlement_asset: &AssetId,
    contract_size: &ContractSize,
    pnl_model: DerivativePnlModel,
    delivery: Option<&DeliveryMode>,
) -> Result<(), ModelError> {
    // What the contract unit has to be follows from what the contract tracks, but
    // the pnl model decides what it may track at all: an inverse contract insists on
    // an asset, so it is refused for that before its unit is read as an index unit.
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

/// Validates which settlement asset a pnl model can reach.
///
/// A linear contract pays the quote price of its underlying and can only settle in
/// that quote; an inverse contract is priced in the quote, delivers the asset it
/// tracks and settles in that asset; a quanto contract pays the quote price and
/// settles somewhere else by definition. The conversion identity is deliberately not
/// part of this judgement: it decides what the contract *may* settle in, and the
/// presence of a conversion follows from that decision rather than the other way
/// round.
///
/// The asset a cash settlement pays in is judged last and by what it is: a settlement
/// the pnl model cannot reach at all is refused for that rather than for the asset, and
/// only a reachable one has to be an asset a payoff can be denominated in. A perpetual
/// has no delivery mode, so this is the only entry that judges the asset a perpetual
/// settles in; the other two families judge their cash delivery while it is decoded.
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
            // An inverse contract is worth `quantity / price` units of the asset it
            // tracks, which a physical delivery of the quote denomination cannot
            // express.
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
    // A physical delivery pays in no asset at all — what it hands over is judged against
    // the underlying instead — so only a cash settlement is judged by what it is
    // denominated in.
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

/// Returns the asset a physical delivery hands over.
///
/// Only an asset can be handed over. An index is the case a caller reaches by choosing a
/// physical delivery for a contract the model can express in cash, and an external
/// reference has no delivered asset either, so both are refused with the reason that
/// names what is missing rather than with a generic leg mismatch.
fn delivered_asset(underlying: &DerivativeUnderlying) -> Result<&AssetId, ModelError> {
    if let Some(asset) = underlying.reference().asset() {
        return Ok(asset);
    }
    Err(match underlying.reference() {
        UnderlyingRef::Index(_) => ModelError::IndexUnderlyingRequiresCashDelivery,
        UnderlyingRef::Asset(_) | UnderlyingRef::External(_) => ModelError::UnderlyingWithoutAsset,
    })
}

/// Validates the physical delivery legs of a future against the asset it tracks.
///
/// A future fixes which asset changes hands and how much of it, and nothing else: it has
/// no right, so the direction of a leg is not part of the contract. The tracked asset
/// therefore has to be delivered at the contract quantity, and a delivery that never
/// names it describes another contract.
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

/// Validates the physical delivery legs of an option against its right and size.
///
/// A physical delivery moves a real asset, so the underlying has to be one, and the leg
/// has to be exactly the underlying at the contract quantity in the direction the right
/// implies: a call receives it and a put delivers it.
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

    /// A contract size that is a notional of the price quote, which is the shape an
    /// inverse contract has.
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

    /// The fixture index as a derivative underlying, priced per index point.
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

    /// The index-point contract size of the spec examples: `I(50,USD,SP/500)`,
    /// a quote notional of 50 USD per index point.
    fn index_contract_size() -> ContractSize {
        ContractSize::new(
            ContractUnit::quote_notional_per_index_point(Quantity::from(50_u8), usd(), index())
                .expect("unit should be valid"),
        )
        .expect("contract size should be valid")
    }

    /// A contract that tracks an oracle feed rather than an asset.
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

    /// One physical delivery of the fixture legs, in canonical order.
    fn physical(deliveries: Vec<DeliveryLeg>) -> DeliveryMode {
        DeliveryMode::physical(deliveries).expect("the legs should form a delivery")
    }

    /// A leg for the price quote of the fixtures.
    fn quote_leg(quantity: u32, direction: DeliveryDirection) -> DeliveryLeg {
        DeliveryLeg::new(usd(), Quantity::from(quantity), direction)
            .expect("the quote leg should be valid")
    }

    /// A leg for the fixture asset.
    fn asset_leg(quantity: u32, direction: DeliveryDirection) -> DeliveryLeg {
        DeliveryLeg::new(venue_token("BTC"), Quantity::from(quantity), direction)
            .expect("the asset leg should be valid")
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

    /// The price reference a quote-settled cash fixture observes.
    fn cash_reference() -> PriceReferenceId {
        reference(
            UnderlyingRef::Asset(venue_token("BTC")),
            usd(),
            PriceBasis::PerAssetUnit,
        )
    }

    /// A strike quoted in the price quote of the fixtures.
    fn strike(value: u32) -> StrikePrice {
        StrikePrice::new(Price::from(value), usd(), PriceBasis::PerAssetUnit)
            .expect("strike should be valid")
    }

    /// One canonical term encoded on its own.
    fn terms_bytes<T: CanonicalTerms>(term: &T) -> Vec<u8> {
        let mut encoder = crate::identifier::CanonicalEncoder::new();
        encoder.write_terms(term);
        encoder.as_bytes().to_vec()
    }

    /// The bytes a cash delivery had before it could carry a conversion reference,
    /// rebuilt from the encoder primitives and the field list of that shape.
    fn previous_cash_delivery_bytes(asset: &AssetId, reference: &PriceReferenceId) -> Vec<u8> {
        let mut encoder = crate::identifier::CanonicalEncoder::new();
        encoder.write_label("cash");
        encoder.write_terms(asset);
        encoder.write_terms(reference);
        encoder.as_bytes().to_vec()
    }

    /// The bytes of a physical delivery, which no conversion reference extends: a
    /// label and the canonical legs.
    fn physical_delivery_bytes(deliveries: &[DeliveryLeg]) -> Vec<u8> {
        let mut encoder = crate::identifier::CanonicalEncoder::new();
        encoder.write_label("physical");
        encoder.write_terms_slice(deliveries);
        encoder.as_bytes().to_vec()
    }

    /// The committed bytes of a cash delivery that carries a conversion reference:
    /// the shape without one followed by the marker and the identity it names.
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

    /// Decodes one wire payload through the conversion the `Deserialize`
    /// implementation uses, so a test can name the typed failure it produced
    /// instead of matching a rendered message.
    fn decode_wire(value: serde_json::Value) -> Result<InstrumentTerms, ModelError> {
        let wire: super::InstrumentWireDto =
            serde_json::from_value(value).expect("the payload should decode into the wire shape");
        InstrumentTerms::try_from(wire)
    }

    /// The typed refusal of one payload, or the rendered decoding error of a field that
    /// never became a value: a rule can refuse a payload while a field of it is decoded or
    /// while the whole terms are validated, and both are refusals.
    fn wire_refusal(value: serde_json::Value) -> Result<ModelError, String> {
        let wire: super::InstrumentWireDto =
            serde_json::from_value(value).map_err(|error| error.to_string())?;
        Ok(InstrumentTerms::try_from(wire)
            .expect_err("the fixture describes no contract this model has"))
    }

    /// Asserts that the public boundary refuses one payload without panicking.
    fn assert_public_decode_refuses(shape: &str, value: serde_json::Value) {
        assert!(
            serde_json::from_value::<Instrument>(value).is_err(),
            "{shape}: the public wire boundary must refuse the payload"
        );
    }

    /// Asserts that one shape has the same outcome through the wire and through the
    /// validated constructor of its family.
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
        // The catalog orders by the full listing key `(route, listing_id)`, and an
        // instrument selector orders by `(name, canonical route, VenueListingId)`,
        // so `Instrument` must stay unordered. The blanket implementation below only
        // exists while `Ord` is absent.
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

        // A linear contract pays the quote price, so settling elsewhere is not a
        // conversion question: it is an unsupported delivery.
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
        // A quote-settled contract has nothing to convert, so a conversion
        // reference is refused even when it would be self-consistent.
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

        // A quanto contract settles in another asset by definition, so an equal
        // settlement asset and quote is not a quanto at all.
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

        // The contract unit is a price-quote notional, not an amount of the
        // underlying: a unit denominated in BTC is the linear shape.
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
        // Settlement is the underlying asset, never the quote.
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
        // An index underlying has no asset to settle in.
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
        // The delivery already carries a conversion, so a constructor that is
        // given none cannot silently drop it: the terms would describe a contract
        // whose settlement asset is not the one the delivery states.
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

        // The reference has to convert the settlement asset into the price quote,
        // so a reference quoted in another asset is the wrong direction.
        assert_eq!(
            quanto(&price_terms(
                UnderlyingRef::Asset(venue_token("USDC")),
                venue_token("USDT"),
                PriceBasis::PerAssetUnit,
            )),
            Err(direction_error(ConversionDirection::Quote))
        );
        // An asset unit conversion is the only basis whose quantity is a plain
        // amount of the settlement asset.
        assert_eq!(
            quanto(&price_terms(
                UnderlyingRef::Asset(venue_token("USDC")),
                usd(),
                PriceBasis::PerContract,
            )),
            Err(direction_error(ConversionDirection::Basis))
        );
        // The underlying half is checkable before any price quote is known, so the
        // delivery mode refuses it on its own.
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

    /// A cash option with a settlement asset that differs from its price quote.
    ///
    /// The option settles in BTC while it is quoted in USD, so it always needs a
    /// conversion reference and it never uses the `Linear => S == Q` rule that
    /// applies to a perp or future.
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
        // A quote-settled cash option needs no conversion.
        assert_eq!(
            cash_option(OptionRight::Call, 100, 1).conversion_reference(),
            None
        );

        // The same shape settling in another asset without a conversion is
        // refused, which is the rule an option must not inherit from `Linear`.
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

        // A cross-asset cash option is valid once the direction is declared.
        let cross_asset = cross_asset_cash_option(OptionRight::Call, 100, 1);
        assert_eq!(
            cross_asset.conversion_reference(),
            Some(
                &conversion(venue_token("BTC"), &usd())
                    .reference_id()
                    .expect("reference")
            )
        );

        // A quote-settled option must not be handed a conversion reference either.
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

        // A cross-asset option cannot be valued from the quote price alone, so a
        // quote observation is refused instead of being reported in BTC units.
        assert_eq!(
            option.cash_payoff(Price::from(120_u32), OptionSettlementObservation::Quote),
            Err(ModelError::RequiredConversionReference)
        );

        // An observation converted through another reference belongs to another
        // contract and must not be accepted here.
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

        // An amount in the wrong asset is not a settlement of this contract.
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

    /// An inverse unit is a notional of the price quote, not an amount of the
    /// underlying, so it cannot be turned into a payoff by a plain quote price.
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

    /// A cash payoff is a quantity of the asset the contract tracks times the price of
    /// that asset, so a contract that tracks no asset at all has no payoff this model can
    /// compute. The rejection is typed, and it never reports the notional as an amount of
    /// the settlement asset.
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

        // An index-point contract is refused for its unit first, because that is what a
        // payoff would have to be denominated in.
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

        // The unit of the external contract is a quantity of the price quote, and the
        // contract tracks no asset of that name either.
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

    /// The three shapes an index contract is named in, and the two
    /// shapes they forbid. A cash settlement is denominated in an asset, and an index
    /// is not one, so tracking an index is a question about the settlement asset and
    /// not about the underlying: the underlying is an index exactly when the contract
    /// unit is the quote notional of one index point of that same index.
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

        // the index perpetual.
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

        // An index can only be valued per index point, so a plain quantity of the
        // index describes a different contract than the underlying does.
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
        // A unit that names another index, and one quoted in an asset the contract is not
        // priced in, are the other two terms of the same rule.
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
        // Nothing delivers an index, so an index contract settles in cash. The reason
        // names that fact instead of claiming the contract cannot settle at all.
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

    /// A contract that tracks an asset states its size as a quantity of assets, so an
    /// index-point unit on it describes a different contract than the underlying does.
    /// Both entries refuse it, and the rejection names what the quantity has to be.
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

    /// A Bermudan option is the exercise style with a fixed early-exercise schedule, and
    /// the schedule is part of the contract: it is inside the expiry, it is canonical, and
    /// it survives the wire.
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

    /// A physical future hands over the asset it tracks, at the quantity the contract
    /// fixes, and nothing else: a future has no right, so the direction of a leg is not
    /// part of the contract while the asset and the quantity are.
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
        // Both directions are the same contract, because a future fixes no right.
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
        // A delivery that never names the tracked asset describes another contract.
        assert_eq!(
            future_of(
                contract_size("BTC", 5),
                physical(vec![quote_leg(500, DeliveryDirection::Deliver)])
            ),
            Err(ModelError::DeliveryLegMismatch)
        );
        // Naming it is not enough: the contract fixes how much of it changes hands.
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

        // The wire decides the same way, because it runs the same entry.
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

    /// An index cannot be handed over, and neither can an external reference: a physical
    /// delivery needs an asset, which is the rule the cash-settled alternative exists for.
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
                // 250 USD per index point, which is what the index unit is quoted in.
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

        // An index future reaches the same refusal on the wire, which is the branch a
        // payload could otherwise take to describe a contract no constructor builds.
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

    /// A settlement asset has to be one a payoff can be denominated in, and a cash
    /// delivery is one of the three places that rule runs. A constructor that owns a
    /// delivery validates it, so the terms it accepts are a subset of the ones a
    /// payload can produce along this dimension too.
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
        // The delivery a caller would have to build by hand to reach this state.
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

        // A quanto future that settles in gold reaches the pnl rules with a settlement
        // they allow, so the only thing left to refuse is the asset the delivery is
        // actually denominated in.
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

        // The same delivery inside a payload reaches the same decision, which is what
        // makes "a constructor accepts no more than the wire does" true here too. The
        // payload's own delivery is gold-settled, and the wire refuses it while that
        // delivery is decoded.
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
        // The payload is refused while its delivery is decoded, which is where the
        // rule runs on the wire; the terms it would have produced are never read.
        let refusal = serde_json::from_value::<Instrument>(wire)
            .expect_err("the wire refuses the commodity settlement")
            .to_string();
        assert!(
            refusal.contains("cannot denominate a settlement for a cash delivery"),
            "{refusal}"
        );
    }

    /// The asset a perpetual settles in has to be one a payoff can be denominated in, and
    /// a perpetual has no delivery that could have judged it. The rule runs in the entry
    /// both the constructor and the wire go through, so a payload cannot settle where the
    /// constructor refuses.
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

        // Every arrangement a perpetual has, each with the conversion its pnl model would
        // need, so the asset the settlement pays in is the only thing left to refuse.
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

    /// One delivery that already declares a conversion, which is the shape a cash
    /// settlement in another asset has.
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

    /// A future whose cash settlement leaves the price quote, and the option of the
    /// same contract, built through the constructors that own the delivery.
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

    /// The delivery declares which conversion the contract settles through, so a
    /// caller that hands over no registry terms describes a contract the delivery
    /// cannot reach. Both families have to refuse that for one reason: a caller that
    /// could drop the delivery's only statement of an identity would install a
    /// cross-asset settlement whose direction nobody established.
    #[test]
    fn a_declared_conversion_should_require_its_terms_from_every_family() {
        let (delivery, declared) = cross_asset_delivery();

        // The delivery alone is not enough, and the reason is the same one on both
        // entries: the terms are what carries the direction.
        let (future, option) = future_and_option_of(delivery.clone(), None);
        assert_eq!(future, Err(ModelError::RequiredConversionReference));
        assert_eq!(option, Err(ModelError::RequiredConversionReference));

        // Terms for another identity are a contradiction, not a replacement.
        let mut other = conversion(venue_token("BTC"), &usd());
        other.observation.kind = ReferenceObservationKind::Index;
        let (future, option) = future_and_option_of(delivery.clone(), Some(&other));
        assert_eq!(future, Err(ModelError::ConversionReferenceMismatch));
        assert_eq!(option, Err(ModelError::ConversionReferenceMismatch));

        // The delivery's own terms are what the caller has to hand over.
        let (future, option) = future_and_option_of(delivery, Some(&declared));
        assert!(future.is_ok(), "the quanto future accepts its own terms");
        let option = option.expect("the cross-asset option accepts its own terms");
        assert_eq!(
            option.conversion_reference(),
            Some(&declared.reference_id().expect("reference"))
        );
    }

    /// A contract that performs no conversion has no identity to store, so neither a
    /// caller's terms nor a delivery that declares one may introduce it. The reason
    /// is about the contract and not about a missing reference, which is what the
    /// same payload on the wire reports.
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

        // A degenerate inverse future: the settlement asset is the price quote, so
        // `S != Q` is false and the contract converts nothing.
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

    /// An outcome contract is the three resolved identities it is made of, and its own
    /// wire shape is exactly those three members: the shape is pinned here so the
    /// decoding entry cannot drift from the encoding one, and an identity that was
    /// never published is refused while the payload is read.
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

    /// A document that is not a set of terms is refused under the name of the terms
    /// themselves: the decoding runs through a private wire shape, and serde names the
    /// type it was asked for in the message a caller reads back.
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

    /// A perpetual without a conversion reference is the shape that existed
    /// before the field did, so its canonical bytes are pinned to the previous
    /// encoding: five terms and nothing else.
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

    /// A perpetual that settles through a conversion reference encodes it after the five
    /// terms of the shape that has none, so the marker and the identity are the whole
    /// difference and both are pinned here.
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

        // A conversion reference the settlement asset does not need is a
        // cross-field contradiction, not a decoding accident.
        let mut unexpected_conversion = encoded.clone();
        unexpected_conversion["spec"]["settlement_asset"] =
            serde_json::to_value(usd()).expect("asset should serialize");
        assert_eq!(
            decode_wire(unexpected_conversion.clone()),
            Err(ModelError::InverseSettlementMismatch),
            "the refusal is the typed rule the constructor states"
        );
        assert!(serde_json::from_value::<Instrument>(unexpected_conversion).is_err());

        // A missing conversion reference for a cross-asset settlement is refused.
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

        // An unknown kind and a truncated spec are decoding errors, not panics.
        let mut unknown_kind = encoded;
        unknown_kind["kind"] = serde_json::Value::String("swap".into());
        assert!(serde_json::from_value::<Instrument>(unknown_kind).is_err());
        assert!(serde_json::from_str::<Instrument>(r#"{"kind":"perpetual"}"#).is_err());
    }

    /// Every product family refuses a malformed payload, and a payload that is well formed
    /// but describes no contract is refused for the typed reason that names the rule it
    /// broke rather than for a decoding accident.
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

        // A spot instrument cannot list one asset against itself.
        let mut identical = spot();
        identical["spec"]["quote"] = identical["spec"]["base"].clone();
        assert_eq!(
            wire_refusal(identical.clone()),
            Ok(ModelError::IdenticalSpotAssets)
        );
        assert_public_decode_refuses("spot against itself", identical);

        // A future whose first notice instant falls after the last trading instant.
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

        // An inverted expiry window is refused while the expiry itself is decoded, which
        // is before any instrument term is read.
        let mut inverted = future();
        inverted["spec"]["expiry"]["last_trading_at"] = serde_json::json!(2_100);
        let refusal = wire_refusal(inverted.clone()).expect_err("the expiry does not decode");
        assert!(
            refusal.contains("last_trading_at must not be later than expiration_at"),
            "{refusal}"
        );
        assert_public_decode_refuses("future with an inverted expiry", inverted);

        // An option whose strike is quoted in another asset than its price quote.
        let mut foreign_strike = option();
        foreign_strike["spec"]["strike"]["quote_asset"] =
            serde_json::to_value(venue_token("USDT")).expect("asset should serialize");
        assert_eq!(
            wire_refusal(foreign_strike.clone()),
            Ok(ModelError::StrikeQuoteMismatch)
        );
        assert_public_decode_refuses("option with a foreign strike quote", foreign_strike);

        // A Bermudan schedule that falls after the expiry, which the option constructor
        // judges once the contract expiry is known.
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

        // A schedule that is not strictly increasing is refused while it is decoded,
        // which is before the contract expiry is read.
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

        // An outcome identity that was never published, which is a decoding error.
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

        // A truncated payload of every family is a decoding error too.
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

    /// A cross-asset cash option needs its conversion, a quote-settled one must not
    /// declare it, and the wire path decides exactly like the constructor: the same
    /// delivery and the same registry terms produce the same verdict at both
    /// entries.
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

        // A quote-settled option whose delivery was patched to carry a conversion.
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

        // The same payload with the instrument-level mirror as well.
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

        // A cross-asset option stripped of the conversion its delivery declared.
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

        // The mirror alone cannot stand in for the delivery that owns the identity.
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
        // Control: the shape both entries accept keeps one identity.
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

    /// The cash delivery of a future is the only place its conversion reference is
    /// stored: a mirror the delivery does not carry is refused, a contradicting one
    /// is refused, and the delivery alone keeps the identity it was built with.
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

        // The delivery drops the identity and the mirror supplies a foreign one.
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

        // Both statements are present and disagree.
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

        // Neither statement is present, which a quanto settlement cannot do.
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

        // The delivery alone is authoritative: an omitted mirror keeps the identity.
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

    /// A cross-asset cash option keeps the identity its delivery states, whether or not
    /// the instrument level repeats it: the mirror is a repetition and never a second
    /// statement, so omitting it and repeating it describe one contract, and a mirror
    /// that contradicts the delivery describes none.
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

        // Two statements of one identity that disagree are a contradiction.
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

        // A mirror that states an identity the delivery does not carry names a conversion
        // the contract does not have.
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

    /// The wire cannot establish what a reference identity means, so the terms a
    /// payload produces must be a subset of the terms the constructors produce:
    /// every shape rule is decided the same way at both entries, and both refuse a
    /// contract the other refuses.
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

        // The shapes every constructor builds survive the wire with one identity.
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

        // A conversion declared where the contract performs none.
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

        // A quote-settled cash option whose delivery declares a conversion, and the
        // same delivery without one: the wire and the constructor agree.
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

    /// An inverse contract whose underlying asset is its own price quote settles in
    /// the quote, which is the shape that carries no conversion at all: only a
    /// *declared* reference is refused.
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

    /// The wire shape of a quote-settled contract is the shape it had before the
    /// conversion reference existed: the member is absent, and only a contract that
    /// carries one serializes it.
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

    /// A cash future without a conversion reference is the shape that existed
    /// before the field did, so its bytes are pinned to that field list: underlying,
    /// price quote, pnl model, contract size, expiry, first notice instant and the
    /// delivery.
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
        // The optional first notice instant writes one presence byte and its value.
        expected.push(1);
        expected.extend_from_slice(&1_700_i64.to_be_bytes());
        expected.extend_from_slice(&previous_cash_delivery_bytes(&usd(), &cash_reference()));

        assert_eq!(encoder.as_bytes(), expected.as_slice());
    }

    /// A physical future never had a conversion reference to encode, so its bytes
    /// are pinned to the same field list it always had.
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
        // A missing first notice instant writes its own presence byte.
        expected.push(0);
        expected.extend_from_slice(&physical_delivery_bytes(&deliveries));

        assert_eq!(encoder.as_bytes(), expected.as_slice());
    }

    /// A quote-settled cash option is the option shape that existed before the
    /// conversion reference did, so its bytes are pinned to that field list.
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

    /// A cross-asset cash future is the quote-settled shape plus the conversion its
    /// delivery declares, so its bytes are pinned to exactly that: the seven terms of
    /// the shape without one, followed by the marker and the identity it names.
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
        // The optional first notice instant writes one presence byte and its value.
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

    /// A cross-asset cash option is pinned the same way: the eight terms of the
    /// quote-settled option shape followed by the conversion its delivery declares.
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

    /// A physical option has no cash settlement to convert, so its bytes are pinned
    /// to the same field list it always had.
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
